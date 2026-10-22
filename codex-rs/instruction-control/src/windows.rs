//! Protected client state. Paths are machine known folders, never environment overrides.
//! The supported boundary is an unelevated Codex process versus the elevated manager.

use crate::PolicySnapshot;
use crate::SourceKind;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
use windows_sys::Win32::Security::ACE_HEADER;
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
use windows_sys::Win32::Security::CheckTokenMembership;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::GetAce;
use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::PSID;
use windows_sys::Win32::Security::SetFileSecurityW;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;
use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Threading::GetExitCodeProcess;
use windows_sys::Win32::System::Threading::WaitForSingleObject;
use windows_sys::Win32::UI::Shell::FOLDERID_ProgramFilesX64;
use windows_sys::Win32::UI::Shell::SEE_MASK_NOCLOSEPROCESS;
use windows_sys::Win32::UI::Shell::SHELLEXECUTEINFOW;
use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
use windows_sys::Win32::UI::Shell::ShellExecuteExW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

mod review_ui;

pub const DISTRIBUTION_BINARIES: [&str; 7] = [
    "codex.exe",
    "codex-policy-manager.exe",
    "codex-code-mode-host.exe",
    "codex-responses-api-proxy.exe",
    "codex-windows-sandbox-setup.exe",
    "codex-windows-sandbox-service.exe",
    "codex-command-runner.exe",
];

const TRUSTED_SIDS: [&str; 3] = [
    "S-1-5-18",
    "S-1-5-32-544",
    // Windows Modules Installer owns the machine Program Files directory on some systems.
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
];

struct LocalAllocation(*mut std::ffi::c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

pub(super) fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn wide_path(path: &Path) -> Vec<u16> {
    let text = path.as_os_str().to_string_lossy();
    if text.starts_with(r"\\?\") || !path.is_absolute() {
        wide(path)
    } else if let Some(network) = text.strip_prefix(r"\\") {
        wide(format!(r"\\?\UNC\{network}"))
    } else {
        wide(format!(r"\\?\{text}"))
    }
}

fn last_error(operation: &str) -> anyhow::Error {
    anyhow::anyhow!("{operation}: {}", std::io::Error::last_os_error())
}

fn sid(value: &str) -> Result<LocalAllocation> {
    let mut result = ptr::null_mut();
    if unsafe { ConvertStringSidToSidW(wide(value).as_ptr(), &mut result) } == 0 {
        return Err(last_error("converting security identifier"));
    }
    Ok(LocalAllocation(result))
}

fn trusted_sid(value: PSID) -> Result<bool> {
    for trusted in TRUSTED_SIDS {
        let trusted = sid(trusted)?;
        if unsafe { EqualSid(value, trusted.0) } != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn reject_reparse(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        let metadata = std::fs::symlink_metadata(ancestor)
            .with_context(|| format!("checking path {}", ancestor.display()))?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            bail!(
                "reparse point not allowed in protected path: {}",
                ancestor.display()
            );
        }
    }
    Ok(())
}

/// Reject owners and allow-ACEs that let an ordinary process modify the object.
/// Unknown ACE types fail closed rather than assuming they are harmless.
fn verify_acl(path: &Path) -> Result<()> {
    reject_reparse(path)?;
    let mut owner = ptr::null_mut();
    let mut acl: *mut ACL = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path(path).as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut acl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        bail!(
            "cannot read protected ACL {}: Windows error {status}",
            path.display()
        );
    }
    let _descriptor = LocalAllocation(descriptor);
    if owner.is_null() || !trusted_sid(owner)? || acl.is_null() {
        bail!("unsafe owner or unrestricted ACL: {}", path.display());
    }
    // FILE_WRITE_DATA/APPEND_DATA/WRITE_EA/DELETE_CHILD/WRITE_ATTRIBUTES,
    // DELETE/WRITE_DAC/WRITE_OWNER and generic write/all/maximum allowed.
    const MUTATING: u32 = 0x0000_0156 | 0x000D_0000 | 0x5200_0000;
    for index in 0..unsafe { (*acl).AceCount } {
        let mut ace = ptr::null_mut();
        if unsafe { GetAce(acl, u32::from(index), &mut ace) } == 0 {
            return Err(last_error("reading access control entry"));
        }
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if header.AceFlags & 0x08 != 0 {
            continue; // INHERIT_ONLY cannot change this object.
        }
        match header.AceType {
            0 => {
                let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
                let trustee = ptr::addr_of!(allowed.SidStart).cast_mut().cast();
                if allowed.Mask & MUTATING != 0 && !trusted_sid(trustee)? {
                    bail!(
                        "ordinary principal can modify protected path: {}",
                        path.display()
                    );
                }
            }
            1 => {} // An ordinary deny ACE grants no permissions.
            _ => bail!(
                "unsupported ACL entry on protected path: {}",
                path.display()
            ),
        }
    }
    Ok(())
}

fn protect(path: &Path) -> Result<()> {
    reject_reparse(path)?;
    let descriptor = wide("O:BAG:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)");
    let mut parsed = ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            1,
            &mut parsed,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error("constructing protected ACL"));
    }
    let _parsed = LocalAllocation(parsed);
    if unsafe {
        SetFileSecurityW(
            wide_path(path).as_ptr(),
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            parsed,
        )
    } == 0
    {
        return Err(last_error("installing protected ACL"));
    }
    verify_acl(path)
}

pub fn install_dir() -> Result<PathBuf> {
    let mut value = ptr::null_mut();
    let status =
        unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFilesX64, 0, ptr::null_mut(), &mut value) };
    if status < 0 || value.is_null() {
        bail!("cannot resolve machine Program Files known folder: HRESULT {status:#x}");
    }
    let length = unsafe {
        (0..)
            .find(|index| *value.add(*index) == 0)
            .context("invalid known folder path")?
    };
    let text = String::from_utf16(unsafe { std::slice::from_raw_parts(value, length) });
    unsafe { CoTaskMemFree(value.cast()) };
    let path = PathBuf::from(text?);
    verify_acl(&path).context("Program Files parent is not protected")?;
    Ok(path.join("CodexStripped"))
}

pub fn policy_dir() -> Result<PathBuf> {
    Ok(install_dir()?.join("Policy"))
}

fn same_path(a: &Path, b: &Path) -> Result<bool> {
    let a = std::fs::canonicalize(a)?;
    let b = std::fs::canonicalize(b)?;
    Ok(a.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy()))
}

fn locked_file(path: &Path) -> Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .with_context(|| format!("opening immutable input {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!("expected an ordinary file: {}", path.display());
    }
    Ok(file)
}

fn read_candidate(path: &Path) -> Result<PolicySnapshot> {
    let mut file = locked_file(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(crate::MAX_SNAPSHOT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    // Parse and retain these exact immutable bytes before the review window is shown.
    PolicySnapshot::parse(&bytes)
}

fn file_digest(file: &mut File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(format!("{:x}", digest.finalize()))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Installation {
    schema_version: u32,
    binaries: BTreeMap<String, String>,
}

fn installation(root: &Path) -> Result<Installation> {
    verify_acl(root)?;
    let path = root.join("installation.json");
    verify_acl(&path)?;
    let manifest: Installation = serde_json::from_reader(locked_file(&path)?)?;
    if manifest.schema_version != 1
        || manifest.binaries.len() != DISTRIBUTION_BINARIES.len()
        || DISTRIBUTION_BINARIES
            .iter()
            .any(|name| !manifest.binaries.contains_key(*name))
    {
        bail!("invalid protected binary installation manifest");
    }
    Ok(manifest)
}

pub fn verify_installed_executable() -> Result<()> {
    let root = install_dir()?;
    let executable = std::env::current_exe()?;
    if !same_path(
        executable.parent().context("executable parent missing")?,
        &root,
    )? {
        bail!(
            "Codex must run from the protected installation {}; use codex-policy-manager install",
            root.display()
        );
    }
    let manifest = installation(&root)?;
    let name = executable
        .file_name()
        .and_then(OsStr::to_str)
        .context("invalid executable name")?;
    let digest = manifest
        .binaries
        .get(name)
        .context("executable is not in protected installation manifest")?;
    verify_acl(&executable)?;
    if file_digest(&mut locked_file(&executable)?)? != *digest {
        bail!("installed executable digest differs from accepted distribution");
    }
    Ok(())
}

fn load_store() -> Result<PolicySnapshot> {
    let root = install_dir()?;
    installation(&root)?;
    let policy = root.join("Policy");
    verify_acl(&policy)?;
    let active = policy.join("active.json");
    verify_acl(&active)?;
    read_candidate(&active)
        .context("protected accepted snapshot missing or invalid; no remote fallback")
}

pub fn load_protected() -> Result<PolicySnapshot> {
    verify_installed_executable()?;
    load_store()
}

pub fn verify_client_permissions() -> Result<()> {
    if is_elevated()? {
        bail!("run client permission verification without administrator rights");
    }
    let snapshot = load_protected()?;
    for source in &snapshot.sources {
        source_path(&snapshot, source.kind.clone(), &source.logical_id)?;
    }
    let root = install_dir()?;
    let manifest = installation(&root)?;
    for name in DISTRIBUTION_BINARIES {
        let path = root.join(name);
        verify_acl(&path)?;
        if file_digest(&mut locked_file(&path)?)? != manifest.binaries[name] {
            bail!("installed binary digest mismatch: {name}");
        }
    }
    // Request mutating rights without writing, truncating, deleting or creating anything.
    for (path, flags) in [
        (root.clone(), FILE_FLAG_BACKUP_SEMANTICS),
        (root.join("Policy/active.json"), 0),
    ] {
        let result = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(0x400D_0156)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(flags | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path);
        match result {
            Err(error) if error.raw_os_error() == Some(5) => {}
            Err(error) => bail!(
                "could not establish denied write access for {}: {error}",
                path.display()
            ),
            Ok(_) => bail!(
                "ordinary client unexpectedly obtained mutating access: {}",
                path.display()
            ),
        }
    }
    println!(
        "Verified protected installation and snapshot {}. Ordinary client mutating access denied; no files changed.",
        snapshot.snapshot_id
    );
    Ok(())
}

pub fn source_path(snapshot: &PolicySnapshot, kind: SourceKind, id: &str) -> Result<PathBuf> {
    let source = snapshot
        .sources
        .iter()
        .find(|source| source.kind == kind && source.logical_id == id)
        .context("instruction source is not accepted")?;
    let content = source.bytes()?;
    let sources = policy_dir()?.join("sources");
    verify_acl(&sources)?;
    let directory = sources.join(&snapshot.snapshot_id);
    verify_acl(&install_dir()?)?;
    verify_acl(&policy_dir()?)?;
    verify_acl(&directory)?;
    let path = directory.join(source.relative_copy_path()?);
    for ancestor in path
        .ancestors()
        .take_while(|ancestor| ancestor.starts_with(&directory))
    {
        verify_acl(ancestor)?;
    }
    let mut actual = Vec::new();
    locked_file(&path)?.read_to_end(&mut actual)?;
    if actual != content.as_ref() {
        bail!("protected source copy differs from accepted snapshot: {id}");
    }
    Ok(path)
}

pub fn is_elevated() -> Result<bool> {
    let administrators = sid("S-1-5-32-544")?;
    let mut member = 0;
    if unsafe { CheckTokenMembership(ptr::null_mut(), administrators.0, &mut member) } == 0 {
        return Err(last_error("checking elevated administrator token"));
    }
    Ok(member != 0)
}

fn quote_argument(value: &OsStr) -> Result<String> {
    let text = value
        .to_str()
        .context("command argument is not valid Unicode")?;
    if text.contains('\0') || text.contains('"') {
        bail!("invalid Windows command argument");
    }
    // CRT quoting: double trailing backslashes before the closing quote.
    let trailing = text.chars().rev().take_while(|c| *c == '\\').count();
    Ok(format!("\"{text}{}\"", "\\".repeat(trailing)))
}

fn elevate(executable: &Path, args: &[&OsStr]) -> Result<()> {
    let arguments = args
        .iter()
        .map(|arg| quote_argument(arg))
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    let executable = wide(executable);
    let arguments = wide(arguments);
    let verb = wide("runas");
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = executable.as_ptr();
    info.lpParameters = arguments.as_ptr();
    info.nShow = SW_SHOWNORMAL;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(last_error("requesting Windows administrator approval"));
    }
    if info.hProcess.is_null() {
        bail!("Windows did not return an elevated manager process");
    }
    let result = (|| -> Result<()> {
        // This wait is inside the interactive helper, not a background agent tool call.
        if unsafe { WaitForSingleObject(info.hProcess, u32::MAX) } != WAIT_OBJECT_0 {
            return Err(last_error("waiting for administrator review"));
        }
        let mut code = 1;
        if unsafe { GetExitCodeProcess(info.hProcess, &mut code) } == 0 {
            return Err(last_error("reading manager result"));
        }
        if code != 0 {
            bail!("administrator review was cancelled or failed (exit {code})");
        }
        Ok(())
    })();
    unsafe { CloseHandle(info.hProcess) };
    result
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("protected file parent missing")?;
    verify_acl(parent)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    drop(output);
    protect(path)
}

fn ensure_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        let parent = path.parent().context("directory parent missing")?;
        verify_acl(parent)?;
        std::fs::create_dir(path)?;
        protect(path)?;
    }
    verify_acl(path)
}

fn persist(root: &Path, snapshot: &PolicySnapshot) -> Result<()> {
    let policy = root.join("Policy");
    ensure_directory(&policy)?;
    let source_root = policy.join("sources");
    ensure_directory(&source_root)?;
    let sources = source_root.join(&snapshot.snapshot_id);
    ensure_directory(&sources)?;
    let history = policy.join("snapshots");
    ensure_directory(&history)?;
    for source in &snapshot.sources {
        let relative = source.relative_copy_path()?;
        let mut directory = sources.clone();
        for component in relative
            .parent()
            .context("source parent missing")?
            .components()
        {
            directory.push(component);
            ensure_directory(&directory)?;
        }
        let path = sources.join(relative);
        let expected = source.bytes()?;
        if path.exists() {
            verify_acl(&path)?;
            let mut content = Vec::new();
            locked_file(&path)?.read_to_end(&mut content)?;
            if content != expected.as_ref() {
                bail!("protected source collision or corruption");
            }
        } else {
            write_new(&path, &expected)?;
        }
    }
    let bytes = serde_json::to_vec_pretty(snapshot)?;
    let archived = history.join(format!("{}.json", snapshot.snapshot_id));
    if !archived.exists() {
        write_new(&archived, &bytes)?;
    } else {
        verify_acl(&archived)?;
        if read_candidate(&archived)? != *snapshot {
            bail!("protected snapshot archive differs");
        }
    }
    let active = policy.join("active.json");
    if active.exists() {
        verify_acl(&active)?;
    }
    let temporary = policy.join(format!(
        "active-{}-{}.tmp",
        std::process::id(),
        snapshot.snapshot_id
    ));
    write_new(&temporary, &bytes)?;
    if unsafe {
        MoveFileExW(
            wide_path(&temporary).as_ptr(),
            wide_path(&active).as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        let error = last_error("atomically replacing accepted snapshot");
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    verify_acl(&active)?;
    Ok(())
}

pub fn apply(candidate: &Path) -> Result<()> {
    reject_reparse(candidate)?;
    let candidate = std::fs::canonicalize(candidate)?;
    let snapshot = read_candidate(&candidate)?;
    if !is_elevated()? {
        let root = install_dir()?;
        let manifest = installation(&root)?;
        let helper = root.join("codex-policy-manager.exe");
        verify_acl(&helper)?;
        if file_digest(&mut locked_file(&helper)?)? != manifest.binaries["codex-policy-manager.exe"]
        {
            bail!("protected manager executable digest mismatch");
        }
        return elevate(
            &helper,
            &[
                OsStr::new("apply"),
                OsStr::new("--candidate"),
                candidate.as_os_str(),
            ],
        );
    }
    verify_installed_executable()?;
    if std::env::current_exe()?.file_name() != Some(OsStr::new("codex-policy-manager.exe")) {
        bail!("only the protected manager can accept instructions");
    }
    let previous = load_store()?;
    let review = crate::review_text(Some(&previous), &snapshot)?;
    review_ui::review("Accept instruction changes", &review)?;
    if load_store()?.snapshot_id != previous.snapshot_id {
        bail!(
            "accepted instructions changed during review; inspect and review the candidate again"
        );
    }
    persist(&install_dir()?, &snapshot)?;
    println!(
        "Accepted snapshot {}. Existing turns retain their previous snapshot.",
        snapshot.snapshot_id
    );
    Ok(())
}

pub fn install(distribution: &Path, candidate: &Path) -> Result<()> {
    reject_reparse(distribution)?;
    reject_reparse(candidate)?;
    let distribution = std::fs::canonicalize(distribution)?;
    let candidate = std::fs::canonicalize(candidate)?;
    let root = install_dir()?;
    if root.exists() {
        bail!(
            "protected installation already exists at {}; use apply for instruction updates",
            root.display()
        );
    }
    let snapshot = read_candidate(&candidate)?;
    let mut inputs = Vec::new();
    let mut manifest = Installation {
        schema_version: 1,
        binaries: BTreeMap::new(),
    };
    for name in DISTRIBUTION_BINARIES {
        let path = distribution.join(name);
        let mut file = locked_file(&path)?;
        let digest = file_digest(&mut file)?;
        manifest.binaries.insert(name.into(), digest);
        inputs.push((name, file));
    }
    // Ensure the privileged implementation being installed is exactly this running helper.
    let mut current_helper = locked_file(&std::env::current_exe()?)?;
    if file_digest(&mut current_helper)? != manifest.binaries["codex-policy-manager.exe"] {
        bail!("distribution manager differs from the running installer");
    }
    if !is_elevated()? {
        return elevate(
            &std::env::current_exe()?,
            &[
                OsStr::new("install"),
                OsStr::new("--distribution"),
                distribution.as_os_str(),
                OsStr::new("--candidate"),
                candidate.as_os_str(),
            ],
        );
    }
    let review = format!(
        "Install into {}\r\n\r\nBinary SHA-256 manifest:\r\n{}\r\n\r\n{}",
        root.display(),
        serde_json::to_string_pretty(&manifest)?,
        crate::review_text(None, &snapshot)?
    );
    review_ui::review("Install protected Codex and instructions", &review)?;
    // All source files remain open without WRITE/DELETE sharing through review and copy.
    std::fs::create_dir(&root)?;
    protect(&root)?;
    for (name, mut input) in inputs {
        let destination = root.join(name);
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        drop(output);
        protect(&destination)?;
        if file_digest(&mut locked_file(&destination)?)? != manifest.binaries[name] {
            bail!("installed binary digest mismatch: {name}");
        }
    }
    write_new(
        &root.join("installation.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    persist(&root, &snapshot)?;
    println!(
        "Installed protected Codex: {}",
        root.join("codex.exe").display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_arguments_do_not_escape_quoted_values() {
        assert_eq!(
            quote_argument(OsStr::new("C:\\path with spaces\\")).unwrap(),
            "\"C:\\path with spaces\\\\\""
        );
        assert!(quote_argument(OsStr::new("x\" --injected")).is_err());
    }

    #[test]
    fn ordinary_user_owned_directory_is_not_a_protected_store() {
        let directory = tempfile::tempdir().unwrap();
        assert!(verify_acl(directory.path()).is_err());
    }

    #[test]
    fn locked_candidate_cannot_be_changed_while_open() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("candidate.json");
        std::fs::write(&path, "original").unwrap();
        let _locked = locked_file(&path).unwrap();
        assert!(std::fs::write(&path, "replacement").is_err());
        assert!(std::fs::remove_file(&path).is_err());
    }
}
