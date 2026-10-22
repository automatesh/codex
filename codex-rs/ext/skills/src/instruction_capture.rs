use std::collections::BTreeMap;
use std::path::Path;

use codex_instruction_control::PolicySource;
use codex_instruction_control::SourceKind;
use codex_skills::SkillMetadata;

use crate::HostSkillsLoadInput;
use crate::HostSkillsService;

const MAX_CAPTURE_FILES: usize = 8192;
const MAX_CAPTURE_BYTES: usize = 48 * 1024 * 1024;

/// Collects a review candidate without running skills, hooks, or model requests.
pub async fn capture_instruction_sources(
    service: &HostSkillsService,
    input: &HostSkillsLoadInput,
) -> Result<Vec<PolicySource>, String> {
    let input = input
        .clone()
        .with_instruction_snapshot(None)
        .with_plugin_skill_snapshots(None);
    let snapshot = service
        .for_request()
        .snapshot_for_cwd(
            &input,
            true,
            Some(std::sync::Arc::clone(&codex_exec_server::LOCAL_FS)),
        )
        .await;
    if !snapshot.outcome().errors.is_empty() {
        return Err(snapshot
            .outcome()
            .errors
            .iter()
            .map(|error| format!("{}: {}", error.path.display(), error.message))
            .collect::<Vec<_>>()
            .join("\n"));
    }
    let mut captured = BTreeMap::new();
    let mut total_bytes = 0;
    for skill in &snapshot.outcome().skills {
        if !snapshot.outcome().is_skill_enabled(skill) {
            continue;
        }
        let path = skill
            .path_to_skills_md
            .to_abs_path()
            .map_err(|error| error.to_string())?;
        let id = path.to_string_lossy().to_string();
        let bytes = read_regular_file(path.as_path())?;
        let content = String::from_utf8(bytes)
            .map_err(|error| format!("Skill {id} is not UTF-8: {error}"))?;
        let source = PolicySource::new(SourceKind::Skill, &id, content);
        insert_source(&mut captured, source, &mut total_bytes)?;
        insert_source(
            &mut captured,
            PolicySource::new(
                SourceKind::SkillMetadata,
                &id,
                serde_json::to_string(skill).map_err(|error| error.to_string())?,
            ),
            &mut total_bytes,
        )?;
        let root = path
            .parent()
            .ok_or_else(|| format!("Skill {id} has no package directory"))?;
        let package_root = input
            .effective_skill_roots
            .iter()
            .find(|root| {
                skill.plugin_id.as_deref() == Some(root.plugin_identity.plugin_id.as_str())
                    && path.starts_with(&root.plugin_root)
            })
            .map(|root| &root.plugin_root)
            .unwrap_or(&root);
        capture_package(
            package_root.as_path(),
            path.as_path(),
            &mut captured,
            &mut total_bytes,
        )?;
        capture_icons(skill, &mut captured, &mut total_bytes)?;
    }
    Ok(captured.into_values().collect())
}

/// Captures an explicitly selected package tree, excluding credential/config files.
pub fn capture_package_assets(root: &Path, kind: SourceKind) -> Result<Vec<PolicySource>, String> {
    let mut captured = BTreeMap::new();
    capture_package(
        root,
        &root.join("__no_main_document__"),
        &mut captured,
        &mut 0,
    )?;
    Ok(captured
        .into_values()
        .map(|mut source| {
            source.kind = kind.clone();
            source
        })
        .collect())
}

fn insert_source(
    captured: &mut BTreeMap<(SourceKind, String), PolicySource>,
    source: PolicySource,
    total_bytes: &mut usize,
) -> Result<(), String> {
    let key = (source.kind.clone(), source.logical_id.clone());
    if let Some(previous) = captured.get(&key) {
        if previous != &source {
            return Err(format!(
                "Source changed during capture: {}",
                source.logical_id
            ));
        }
        return Ok(());
    }
    *total_bytes += source.content.len();
    if captured.len() >= MAX_CAPTURE_FILES || *total_bytes > MAX_CAPTURE_BYTES {
        return Err(format!(
            "Skill candidate exceeds 8192 files or 48 MiB: {} sources, {} encoded bytes at {}; no partial package was accepted",
            captured.len() + 1,
            total_bytes,
            source.logical_id,
        ));
    }
    captured.insert(key, source);
    Ok(())
}

fn capture_package(
    root: &Path,
    main: &Path,
    captured: &mut BTreeMap<(SourceKind, String), PolicySource>,
    total_bytes: &mut usize,
) -> Result<(), String> {
    let mut pending = vec![root.to_path_buf()];
    let mut visited = 0;
    while let Some(directory) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&directory)
            .map_err(|error| format!("{}: {error}", directory.display()))?;
        reject_reparse(&directory, &metadata)?;
        for entry in std::fs::read_dir(&directory).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            visited += 1;
            if visited > MAX_CAPTURE_FILES {
                return Err(format!(
                    "Skill package {} exceeds the bounded file count",
                    root.display()
                ));
            }
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
            reject_reparse(&path, &metadata)?;
            if metadata.is_dir() {
                if path.file_name().and_then(|name| name.to_str()) == Some(".git") {
                    continue;
                }
                pending.push(path);
            } else if path != main {
                if matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(".mcp.json" | ".app.json")
                ) {
                    continue;
                }
                if sensitive_file_name(&path) {
                    return Err(format!(
                        "Potential credential file {} was not copied; explicitly select a package without credential files",
                        path.display()
                    ));
                }
                let bytes = read_regular_file(&path)?;
                insert_source(
                    captured,
                    PolicySource::asset(SourceKind::SkillAsset, path.to_string_lossy(), bytes),
                    total_bytes,
                )?;
            }
        }
    }
    Ok(())
}

fn capture_icons(
    skill: &SkillMetadata,
    captured: &mut BTreeMap<(SourceKind, String), PolicySource>,
    total_bytes: &mut usize,
) -> Result<(), String> {
    if let Some(interface) = &skill.interface {
        for path in [&interface.icon_small, &interface.icon_large]
            .into_iter()
            .flatten()
        {
            insert_source(
                captured,
                PolicySource::asset(
                    SourceKind::SkillAsset,
                    path.to_string_lossy(),
                    read_regular_file(path.as_path())?,
                ),
                total_bytes,
            )?;
        }
    }
    Ok(())
}

fn sensitive_file_name(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    name == ".env"
        || name.starts_with(".env.")
        || matches!(
            name.as_str(),
            "auth.json"
                | "credentials.json"
                | "credentials"
                | "id_rsa"
                | "id_ed25519"
                | "token.json"
        )
}

fn reject_reparse(path: &Path, metadata: &std::fs::Metadata) -> Result<(), String> {
    let linked = metadata.file_type().is_symlink();
    #[cfg(windows)]
    let linked = {
        use std::os::windows::fs::MetadataExt;
        linked || metadata.file_attributes() & 0x400 != 0
    };
    if linked {
        return Err(format!(
            "Skill asset {} is a symlink or reparse point; package capture does not follow it",
            path.display()
        ));
    }
    Ok(())
}

fn read_regular_file(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    reject_reparse(path, &metadata)?;
    if !metadata.is_file() || metadata.len() > MAX_CAPTURE_BYTES as u64 {
        return Err(format!(
            "Skill asset {} is not a bounded regular file",
            path.display()
        ));
    }
    std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_binary_assets_and_relative_package_layout() {
        let directory = tempfile::tempdir().unwrap();
        let main = directory.path().join("SKILL.md");
        std::fs::write(&main, "main").unwrap();
        std::fs::create_dir(directory.path().join("scripts")).unwrap();
        let script = directory.path().join("scripts/run.py");
        std::fs::write(&script, "print('ok')").unwrap();
        let image = directory.path().join("image.png");
        std::fs::write(&image, [0, 255, 1]).unwrap();
        let mut sources = BTreeMap::new();
        capture_package(directory.path(), &main, &mut sources, &mut 0).unwrap();
        assert_eq!(sources.len(), 2);
        assert!(
            sources.contains_key(&(SourceKind::SkillAsset, script.to_string_lossy().to_string()))
        );
        assert_eq!(
            sources[&(SourceKind::SkillAsset, image.to_string_lossy().to_string())]
                .bytes()
                .unwrap()
                .as_ref(),
            &[0, 255, 1]
        );
    }

    #[test]
    fn refuses_credential_files_and_conflicting_capture() {
        let directory = tempfile::tempdir().unwrap();
        let main = directory.path().join("SKILL.md");
        std::fs::write(&main, "main").unwrap();
        std::fs::write(directory.path().join("auth.json"), "sensitive test fixture").unwrap();
        assert!(capture_package(directory.path(), &main, &mut BTreeMap::new(), &mut 0).is_err());
        let mut sources = BTreeMap::new();
        insert_source(
            &mut sources,
            PolicySource::new(SourceKind::Skill, "same", "first".into()),
            &mut 0,
        )
        .unwrap();
        assert!(
            insert_source(
                &mut sources,
                PolicySource::new(SourceKind::Skill, "same", "changed".into()),
                &mut 0
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_links_to_assets_outside_the_package() {
        let directory = tempfile::tempdir().unwrap();
        let main = directory.path().join("SKILL.md");
        std::fs::write(&main, "main").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", directory.path().join("reference")).unwrap();
        assert!(capture_package(directory.path(), &main, &mut BTreeMap::new(), &mut 0).is_err());
    }
}
