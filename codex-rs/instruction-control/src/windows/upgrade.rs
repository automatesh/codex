use super::*;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;

pub(super) const IN_PROGRESS: &str = "binary-upgrade-in-progress.json";
const STAGING: &str = "binary-upgrade-staging";
const MANIFEST: &str = "installation.json";
const MAX_ERROR_BYTES: usize = 32 * 1024;

fn error_log_path(root: &Path, pid: u32) -> Result<PathBuf> {
    if pid == 0 {
        bail!("Windows did not return an elevated child process ID");
    }
    Ok(root.join(format!("binary-upgrade-error-{pid}.log")))
}

fn bounded_error(error: &anyhow::Error) -> String {
    let mut message = format!("{error:#}");
    if message.len() > MAX_ERROR_BYTES {
        const SUFFIX: &str = "\n[error truncated]";
        let mut end = MAX_ERROR_BYTES - SUFFIX.len();
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push_str(SUFFIX);
    }
    message
}

fn record_error(root: &Path, pid: u32, error: &anyhow::Error) -> Result<()> {
    verify_acl(root)?;
    let path = error_log_path(root, pid)?;
    // Existing paths are never followed or replaced, including stale PID-reuse records.
    write_new(&path, bounded_error(error).as_bytes())
}

pub(super) fn read_error_log(root: &Path, pid: u32, child_created: u64) -> Result<Option<String>> {
    verify_acl(root)?;
    let path = error_log_path(root, pid)?;
    if !path.try_exists()? {
        return Ok(None);
    }
    verify_acl(&path)?;
    let input = locked_file(&path)?;
    if input.metadata()?.creation_time() < child_created {
        bail!("protected upgrade diagnostic belongs to an earlier use of this process ID");
    }
    let mut bytes = Vec::new();
    input
        .take(MAX_ERROR_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_ERROR_BYTES {
        bail!("protected upgrade diagnostic exceeds the byte limit");
    }
    Ok(Some(String::from_utf8(bytes)?))
}

fn replacement_names() -> impl Iterator<Item = &'static str> {
    DISTRIBUTION_BINARIES.into_iter().chain([MANIFEST])
}

fn move_replace(source: &Path, destination: &Path) -> Result<()> {
    if unsafe {
        MoveFileExW(
            wide_path(source).as_ptr(),
            wide_path(destination).as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(last_error("replacing installed binary"))
            .with_context(|| format!("{} -> {}", source.display(), destination.display()));
    }
    Ok(())
}

fn copy_locked(input: &mut File, destination: &Path, expected: &str) -> Result<()> {
    input.seek(SeekFrom::Start(0))?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    std::io::copy(input, &mut output)?;
    output.sync_all()?;
    drop(output);
    protect(destination)?;
    if file_digest(&mut locked_file(destination)?)? != expected {
        bail!("staged binary digest mismatch: {}", destination.display());
    }
    Ok(())
}

fn verify_binaries(root: &Path, manifest: &Installation) -> Result<()> {
    for name in DISTRIBUTION_BINARIES {
        let path = root.join(name);
        verify_acl(&path)?;
        if file_digest(&mut locked_file(&path)?)? != manifest.binaries[name] {
            bail!("installed binary digest mismatch: {name}");
        }
    }
    Ok(())
}

// Request write/delete rights without changing bytes: Windows refuses an executing image
// or an incompatible reader before any installed file is replaced.
fn preflight_closed_binaries(root: &Path) -> Result<()> {
    let mut handles = Vec::new();
    for name in DISTRIBUTION_BINARIES {
        let path = root.join(name);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .access_mode(0xC001_0000)
            .share_mode(FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
            .with_context(|| {
                format!(
                    "installed binary is in use or cannot be replaced: {}; close installed Codex, helper, and SDK processes before upgrading",
                    path.display()
                )
            })?;
        handles.push(file);
    }
    Ok(())
}

fn replace_with_rollback(
    root: &Path,
    staging: &Path,
    mut replace: impl FnMut(&Path, &Path) -> Result<()>,
    validate: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let mut changed = Vec::new();
    let outcome = (|| -> Result<()> {
        for name in replacement_names() {
            replace(&staging.join("new").join(name), &root.join(name))?;
            changed.push(name);
        }
        validate()
    })();
    if let Err(error) = outcome {
        let mut rollback_errors = Vec::new();
        for name in changed.into_iter().rev() {
            if let Err(rollback) = replace(&staging.join("old").join(name), &root.join(name)) {
                rollback_errors.push(format!("{name}: {rollback:#}"));
            }
        }
        if !rollback_errors.is_empty() {
            bail!(
                "upgrade failed: {error:#}; rollback incomplete: {}; the in-progress marker and protected backups were retained",
                rollback_errors.join("; ")
            );
        }
        return Err(error).context("upgrade failed; original installed bytes restored");
    }
    Ok(())
}

fn validate_preserved_snapshot(root: &Path, expected: &PolicySnapshot) -> Result<()> {
    let active = root.join("Policy").join("active.json");
    verify_acl(&active)?;
    if read_candidate(&active)? != *expected {
        bail!("accepted instructions changed during binary upgrade");
    }
    Ok(())
}

fn remove_staging(root: &Path, staging: &Path) -> Result<()> {
    reject_reparse(staging)?;
    let resolved = std::fs::canonicalize(staging)?;
    if resolved.file_name() != Some(OsStr::new(STAGING))
        || !same_path(resolved.parent().context("staging parent missing")?, root)?
    {
        bail!("refusing cleanup outside the protected binary staging directory");
    }
    std::fs::remove_dir_all(&resolved)?;
    Ok(())
}

/// Upgrade only binaries. Neither accepted instruction state nor the user's .codex is written.
pub fn upgrade(distribution: &Path) -> Result<()> {
    let result = upgrade_inner(distribution);
    if let Err(error) = &result
        && is_elevated().unwrap_or(false)
        && let Ok(root) = install_dir()
    {
        // Reporting must never replace the actual failure or require another privilege action.
        let _ = record_error(&root, std::process::id(), error);
    }
    result
}

fn upgrade_inner(distribution: &Path) -> Result<()> {
    reject_reparse(distribution)?;
    let distribution = std::fs::canonicalize(distribution)?;
    let root = install_dir()?;
    if same_path(&distribution, &root)? {
        bail!("run upgrade from the new distribution outside the protected installation");
    }
    let mut next = Installation {
        schema_version: 1,
        binaries: BTreeMap::new(),
    };
    let mut inputs = Vec::new();
    for name in DISTRIBUTION_BINARIES {
        let mut input = locked_file(&distribution.join(name))?;
        next.binaries.insert(name.into(), file_digest(&mut input)?);
        inputs.push((name, input));
    }
    let executable = std::env::current_exe()?;
    if file_digest(&mut locked_file(&executable)?)? != next.binaries["codex-policy-manager.exe"] {
        bail!("distribution manager differs from the running upgrade helper");
    }
    if !is_elevated()? {
        return elevate(
            &executable,
            &[
                OsStr::new("upgrade"),
                OsStr::new("--distribution"),
                distribution.as_os_str(),
            ],
        );
    }
    let _update_lock = acquire_update_lock(&root)?;
    let previous = installation(&root)?;
    verify_binaries(&root, &previous)?;
    let snapshot = load_store()?;
    // Validate every accepted copy before review; upgrade does not reconstruct any of them.
    for source in &snapshot.sources {
        source_path(&snapshot, source.kind.clone(), &source.logical_id)?;
    }
    preflight_closed_binaries(&root)?;
    let review = format!(
        "Upgrade protected Codex binaries in {}\r\n\r\nAccepted instructions remain unchanged: {}\r\nNo Policy source, snapshot, or user .codex content will be replaced.\r\n\r\nCURRENT binary SHA-256 manifest:\r\n{}\r\n\r\nPROPOSED binary SHA-256 manifest:\r\n{}",
        root.display(),
        snapshot.snapshot_id,
        serde_json::to_string_pretty(&previous)?,
        serde_json::to_string_pretty(&next)?
    );
    review_ui::review("Accept Codex binary upgrade", &review)?;
    validate_preserved_snapshot(&root, &snapshot)?;
    verify_binaries(&root, &previous)?;
    preflight_closed_binaries(&root)?;
    let staging = root.join(STAGING);
    let mut previous_manifest_bytes = Vec::new();
    locked_file(&root.join(MANIFEST))?.read_to_end(&mut previous_manifest_bytes)?;
    let next_manifest_bytes = serde_json::to_vec_pretty(&next)?;
    // Never reuse leftovers from another transaction or an interrupted administrator action.
    std::fs::create_dir(&staging).context(
        "cannot exclusively create binary upgrade staging; an interrupted upgrade may need administrator recovery",
    )?;
    protect(&staging)?;
    let staged = (|| -> Result<()> {
        for directory in [staging.join("new"), staging.join("old")] {
            std::fs::create_dir(&directory)?;
            protect(&directory)?;
        }
        for (name, input) in &mut inputs {
            let name = *name;
            copy_locked(input, &staging.join("new").join(name), &next.binaries[name])?;
            copy_locked(
                &mut locked_file(&root.join(name))?,
                &staging.join("old").join(name),
                &previous.binaries[name],
            )?;
        }
        write_new(&staging.join("new").join(MANIFEST), &next_manifest_bytes)?;
        write_new(
            &staging.join("old").join(MANIFEST),
            &previous_manifest_bytes,
        )?;
        Ok(())
    })();
    if let Err(error) = staged {
        let _ = remove_staging(&root, &staging);
        return Err(error).context("binary staging failed; installed bytes remain unchanged");
    }
    let marker = root.join(IN_PROGRESS);
    write_new(
        &marker,
        &serde_json::to_vec_pretty(&serde_json::json!({
            "accepted_snapshot": snapshot.snapshot_id,
            "previous": previous,
            "proposed": next,
        }))?,
    )?;
    let result = replace_with_rollback(&root, &staging, move_replace, || {
        verify_binaries(&root, &next)?;
        verify_acl(&root.join(MANIFEST))?;
        if std::fs::read(root.join(MANIFEST))? != next_manifest_bytes {
            bail!("installed binary manifest differs from reviewed distribution");
        }
        validate_preserved_snapshot(&root, &snapshot)
    });
    if let Err(error) = result {
        // Remove the marker only after independently proving complete restoration.
        if verify_binaries(&root, &previous).is_ok()
            && std::fs::read(root.join(MANIFEST))? == previous_manifest_bytes
            && validate_preserved_snapshot(&root, &snapshot).is_ok()
        {
            std::fs::remove_file(&marker)?;
            let _ = remove_staging(&root, &staging);
        }
        return Err(error);
    }
    std::fs::remove_file(&marker)?;
    remove_staging(&root, &staging)
        .context("binaries were upgraded successfully, but protected staging cleanup failed")?;
    println!(
        "Upgraded protected Codex binaries. Accepted snapshot {} unchanged. Start Codex without administrator rights.",
        snapshot.snapshot_id
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_diagnostic_is_bounded_and_preserves_utf8_and_error_chain() {
        let error = anyhow::anyhow!("underlying Windows failure").context("operation failed");
        assert_eq!(
            bounded_error(&error),
            "operation failed: underlying Windows failure"
        );
        let long_error = anyhow::anyhow!("failure {}", "\u{1f6a8}".repeat(MAX_ERROR_BYTES));
        let bounded = bounded_error(&long_error);
        assert!(bounded.len() <= MAX_ERROR_BYTES);
        assert!(bounded.ends_with("[error truncated]"));
    }

    #[test]
    fn upgrade_diagnostic_path_uses_only_a_nonzero_child_pid() {
        let root = Path::new("C:/Program Files/CodexStripped");
        assert_eq!(
            error_log_path(root, 12345).unwrap(),
            root.join("binary-upgrade-error-12345.log")
        );
        assert!(error_log_path(root, 0).is_err());
    }

    #[test]
    fn upgrade_diagnostic_rejects_unprotected_storage_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let error = anyhow::anyhow!("test failure");
        assert!(record_error(directory.path(), 12345, &error).is_err());
        assert!(read_error_log(directory.path(), 12345, 0).is_err());
        assert!(!error_log_path(directory.path(), 12345).unwrap().exists());
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("installation");
        let staging = root.join(STAGING);
        std::fs::create_dir_all(staging.join("old")).unwrap();
        std::fs::create_dir_all(staging.join("new")).unwrap();
        std::fs::create_dir(root.join("Policy")).unwrap();
        std::fs::write(root.join("Policy/active.json"), b"accepted untouched").unwrap();
        for name in replacement_names() {
            std::fs::write(root.join(name), format!("original {name}")).unwrap();
            std::fs::write(staging.join("old").join(name), format!("original {name}")).unwrap();
            std::fs::write(staging.join("new").join(name), format!("new {name}")).unwrap();
        }
        (temporary, root, staging)
    }

    fn assert_original(root: &Path) {
        for name in replacement_names() {
            assert_eq!(
                std::fs::read(root.join(name)).unwrap(),
                format!("original {name}").as_bytes()
            );
        }
        assert_eq!(
            std::fs::read(root.join("Policy/active.json")).unwrap(),
            b"accepted untouched"
        );
    }

    #[test]
    fn binary_upgrade_replaces_all_binaries_and_preserves_policy() {
        let (_temporary, root, staging) = fixture();
        replace_with_rollback(&root, &staging, move_replace, || Ok(())).unwrap();
        for name in replacement_names() {
            assert_eq!(
                std::fs::read(root.join(name)).unwrap(),
                format!("new {name}").as_bytes()
            );
        }
        assert_eq!(
            std::fs::read(root.join("Policy/active.json")).unwrap(),
            b"accepted untouched"
        );
    }

    #[test]
    fn failure_after_partial_binary_upgrade_restores_original_bytes() {
        let (_temporary, root, staging) = fixture();
        let fail_at = staging.join("new").join(DISTRIBUTION_BINARIES[3]);
        let error = replace_with_rollback(
            &root,
            &staging,
            |source, destination| {
                if source == fail_at {
                    bail!("simulated replacement failure");
                }
                move_replace(source, destination)
            },
            || Ok(()),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("original installed bytes restored"));
        assert_original(&root);
    }

    #[test]
    fn failed_final_validation_restores_original_manifest_and_binaries() {
        let (_temporary, root, staging) = fixture();
        assert!(
            replace_with_rollback(&root, &staging, move_replace, || {
                bail!("simulated validation failure")
            })
            .is_err()
        );
        assert_original(&root);
    }

    #[test]
    fn busy_installed_binary_fails_before_modification() {
        let (_temporary, root, _staging) = fixture();
        let _busy = locked_file(&root.join(DISTRIBUTION_BINARIES[4])).unwrap();
        assert!(preflight_closed_binaries(&root).is_err());
        assert_original(&root);
    }

    #[test]
    fn incomplete_rollback_retains_marker_and_unrestored_backup() {
        let (_temporary, root, staging) = fixture();
        std::fs::write(root.join(IN_PROGRESS), b"pending reviewed upgrade").unwrap();
        let fail_new = staging.join("new").join(DISTRIBUTION_BINARIES[3]);
        let fail_restore = staging.join("old").join(DISTRIBUTION_BINARIES[2]);
        let error = replace_with_rollback(
            &root,
            &staging,
            |source, destination| {
                if source == fail_new || source == fail_restore {
                    bail!("simulated locked replacement");
                }
                move_replace(source, destination)
            },
            || Ok(()),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("rollback incomplete"));
        assert_eq!(
            std::fs::read(root.join(IN_PROGRESS)).unwrap(),
            b"pending reviewed upgrade"
        );
        assert!(fail_restore.exists());
        assert_eq!(
            std::fs::read(root.join("Policy/active.json")).unwrap(),
            b"accepted untouched"
        );
    }

    #[test]
    fn staging_cleanup_rejects_directory_outside_fixed_installation() {
        let (temporary, root, _staging) = fixture();
        let unrelated = temporary.path().join("unrelated").join(STAGING);
        std::fs::create_dir_all(&unrelated).unwrap();
        let keep = unrelated.join("keep");
        std::fs::write(&keep, b"unrelated file").unwrap();
        assert!(remove_staging(&root, &unrelated).is_err());
        assert_eq!(std::fs::read(keep).unwrap(), b"unrelated file");
    }
}
