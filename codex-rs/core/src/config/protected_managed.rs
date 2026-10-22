use super::Config;
use super::LoaderOverrides;
use codex_config::CloudConfigBundleLoader;
use codex_config::ConfigLayerSource;
use codex_config::RequirementSource;
use codex_config::RequirementsLayerEntry;
use codex_exec_server::LOCAL_FS;
use codex_file_system::CopyOptions;
use codex_file_system::CreateDirectoryOptions;
use codex_file_system::ExecutorFileSystem;
use codex_file_system::ExecutorFileSystemFuture;
use codex_file_system::FileMetadata;
use codex_file_system::FileSystemReadStream;
use codex_file_system::FileSystemSandboxContext;
use codex_file_system::GetMetadataOptions;
use codex_file_system::ReadDirectoryEntry;
use codex_file_system::ReadFileOptions;
use codex_file_system::RemoveOptions;
use codex_file_system::WalkOptions;
use codex_file_system::WalkOutcome;
use codex_file_system::WriteFileOptions;
use codex_instruction_control::ManagedRequirements;
use codex_instruction_control::PolicySnapshot;
use codex_instruction_control::PolicySource;
use codex_instruction_control::SourceKind;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::path::PathBuf;

pub(super) struct PinnedManagedFileSystem {
    paths: HashMap<PathUri, Option<Vec<u8>>>,
}

fn approval_required(id: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "managed instruction source requires explicit acceptance: {id}; capture it into a candidate and approve with codex-policy-manager"
        ),
    )
}

fn local_path(id: &str) -> io::Result<PathBuf> {
    let path = PathBuf::from(id);
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("managed source ID must be an absolute local path or cloud:<id>: {id}"),
        ));
    }
    Ok(path)
}

fn same_id(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.replace('\\', "/").to_lowercase() == b.replace('\\', "/").to_lowercase()
    } else {
        a == b
    }
}

fn path_uri(path: &Path) -> io::Result<PathUri> {
    Ok(PathUri::from_abs_path(
        &AbsolutePathBuf::from_absolute_path(path)?,
    ))
}

fn source_at<'a>(
    snapshot: &'a PolicySnapshot,
    kind: SourceKind,
    path: &Path,
) -> Option<&'a PolicySource> {
    let id = path.to_string_lossy();
    snapshot
        .sources
        .iter()
        .find(|source| source.kind == kind && same_id(&source.logical_id, &id))
}

fn default_requirements_path() -> io::Result<PathBuf> {
    Ok(codex_config::loader::system_config_toml_file()?
        .as_path()
        .with_file_name("requirements.toml"))
}

fn legacy_path(codex_home: &Path, overrides: &LoaderOverrides) -> Option<PathBuf> {
    if let Some(path) = &overrides.managed_config_path {
        return Some(path.clone());
    }
    #[cfg(unix)]
    {
        let _ = codex_home;
        Some(PathBuf::from("/etc/codex/managed_config.toml"))
    }
    #[cfg(windows)]
    {
        let _ = codex_home;
        None
    }
}

async fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match LOCAL_FS
        .read_file(&path_uri(path)?, ReadFileOptions::default(), None)
        .await
    {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Pin local bytes without changing their path-resolution semantics. A live mandatory
/// requirement with no accepted counterpart fails rather than being silently omitted.
pub(super) async fn prepare(
    snapshot: &PolicySnapshot,
    codex_home: &Path,
    mut overrides: LoaderOverrides,
    incoming_cloud: CloudConfigBundleLoader,
) -> io::Result<(
    PinnedManagedFileSystem,
    LoaderOverrides,
    CloudConfigBundleLoader,
)> {
    let mut paths = HashMap::new();
    let system = match &overrides.system_config_path {
        Some(path) => path.clone(),
        None => codex_config::loader::system_config_toml_file()?.to_path_buf(),
    };
    let requirements_path = overrides
        .system_requirements_path
        .clone()
        .unwrap_or(default_requirements_path()?);

    for (kind, path, mandatory) in [
        (SourceKind::ManagedConfig, Some(system.clone()), false),
        (
            SourceKind::ManagedConfig,
            legacy_path(codex_home, &overrides),
            true,
        ),
        (
            SourceKind::ManagedRequirements,
            Some(requirements_path.clone()),
            true,
        ),
    ] {
        let Some(path) = path else {
            continue;
        };
        let accepted = source_at(snapshot, kind, &path);
        let live = read_optional(&path).await?;
        if let Some(live) = live.as_deref().filter(|bytes| !bytes.is_empty()) {
            if mandatory {
                if accepted.is_none_or(|source| {
                    source.bytes().map_or(true, |bytes| bytes.as_ref() != live)
                }) {
                    return Err(approval_required(&path.to_string_lossy()));
                }
            } else if accepted.is_none() {
                return Err(approval_required(&path.to_string_lossy()));
            }
        }
        paths.insert(
            path_uri(&path)?,
            accepted
                .map(|source| source.bytes().map(|bytes| bytes.into_owned()))
                .transpose()
                .map_err(io::Error::other)?,
        );
    }

    let accepted_requirements = snapshot
        .sources
        .iter()
        .filter(|source| {
            source.kind == SourceKind::ManagedRequirements
                && !source.logical_id.starts_with("cloud:")
        })
        .collect::<Vec<_>>();
    if accepted_requirements.len() > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "multiple accepted local requirements files have no explicit precedence; combine them into one reviewed requirements source",
        ));
    }
    let mut requirements = Vec::new();
    for source in &accepted_requirements {
        let path = local_path(&source.logical_id)?;
        let base = AbsolutePathBuf::from_absolute_path(path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "managed requirements source parent missing",
            )
        })?)?;
        requirements.push(
            RequirementsLayerEntry::from_toml(
                RequirementSource::SystemRequirementsToml {
                    file: AbsolutePathBuf::from_absolute_path(&path)?,
                },
                String::from_utf8(source.bytes().map_err(io::Error::other)?.into_owned())
                    .map_err(io::Error::other)?,
            )
            .with_base_dir(base),
        );
    }
    let composed = codex_config::compose_requirements(requirements)?.unwrap_or_default();
    let _: codex_config::ConfigRequirements = composed.try_into()?;
    if let Some(source) = accepted_requirements.first() {
        let source_path = local_path(&source.logical_id)?;
        paths.insert(
            path_uri(&source_path)?,
            Some(source.bytes().map_err(io::Error::other)?.into_owned()),
        );
        overrides.system_requirements_path = Some(source_path);
    } else {
        overrides.system_requirements_path = Some(requirements_path);
    }
    overrides.ignore_managed_requirements = false;
    overrides.ignore_login_requirements = false;

    for source in snapshot.sources.iter().filter(|source| {
        source.kind == SourceKind::ManagedConfig && !source.logical_id.starts_with("cloud:")
    }) {
        let path = local_path(&source.logical_id)?;
        if same_id(&source.logical_id, &system.to_string_lossy()) {
            continue;
        }
        if path.file_name() != Some(std::ffi::OsStr::new("managed_config.toml")) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unrecognized local managed config source {}; use the active system config path or managed_config.toml",
                    source.logical_id
                ),
            ));
        }
        if overrides
            .managed_config_path
            .as_ref()
            .is_some_and(|existing| existing != &path)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "multiple legacy managed configuration sources require an explicit precedence definition",
            ));
        }
        overrides.managed_config_path = Some(path.clone());
        paths.insert(
            path_uri(&path)?,
            Some(source.bytes().map_err(io::Error::other)?.into_owned()),
        );
    }

    // Discovery may fetch the managed bundle, but no fragment is admitted until its
    // exact bytes match the accepted source. Keep the server's live authority binding.
    let cloud_snapshot = incoming_cloud.get_snapshot().await;
    let bundle = cloud_snapshot
        .bundle
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))?;
    let mut live_cloud_ids = std::collections::BTreeSet::new();
    if let Some(bundle) = bundle {
        if !bundle.is_empty() {
            if snapshot.config.controls.managed_requirements != ManagedRequirements::ApprovedSources
            {
                return Err(approval_required("cloud:bundle"));
            }
            let manifest = serde_json::to_string(bundle).map_err(io::Error::other)?;
            snapshot
                .verify_content(SourceKind::ManagedBundle, "cloud:bundle", &manifest)
                .map_err(|_| approval_required("cloud:bundle (ordered fragment manifest)"))?;
            live_cloud_ids.insert((SourceKind::ManagedBundle, "cloud:bundle".into()));
        }
        for (kind, id, content) in bundle
            .config_toml
            .enterprise_managed
            .iter()
            .map(|fragment| (SourceKind::ManagedConfig, &fragment.id, &fragment.contents))
            .chain(
                bundle
                    .requirements_toml
                    .enterprise_managed
                    .iter()
                    .map(|fragment| {
                        (
                            SourceKind::ManagedRequirements,
                            &fragment.id,
                            &fragment.contents,
                        )
                    }),
            )
        {
            let id = format!("cloud:{id}");
            if snapshot.config.controls.managed_requirements != ManagedRequirements::ApprovedSources
            {
                return Err(approval_required(&id));
            }
            snapshot
                .verify_content(kind.clone(), &id, content)
                .map_err(|_| approval_required(&id))?;
            live_cloud_ids.insert((kind, id));
        }
    }
    for source in snapshot.sources.iter().filter(|source| {
        matches!(
            source.kind,
            SourceKind::ManagedConfig | SourceKind::ManagedRequirements | SourceKind::ManagedBundle
        ) && source.logical_id.starts_with("cloud:")
    }) {
        if snapshot.config.controls.managed_requirements != ManagedRequirements::ApprovedSources {
            return Err(approval_required(&source.logical_id));
        }
        if !live_cloud_ids.contains(&(source.kind.clone(), source.logical_id.clone())) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "accepted enterprise source {} is not currently delivered; refusing to invent or remove enterprise requirements",
                    source.logical_id
                ),
            ));
        }
    }
    Ok((
        PinnedManagedFileSystem { paths },
        overrides,
        CloudConfigBundleLoader::from_snapshot(cloud_snapshot),
    ))
}

/// Capture current managed sources for review. This does not accept or execute them.
pub async fn capture_managed_instruction_sources(config: &Config) -> io::Result<Vec<PolicySource>> {
    let mut sources = Vec::new();
    for layer in config.config_layer_stack.layers_low_to_high() {
        let (kind, id) = match &layer.name {
            ConfigLayerSource::System { file }
            | ConfigLayerSource::LegacyManagedConfigTomlFromFile { file } => (
                SourceKind::ManagedConfig,
                file.as_path().to_string_lossy().into_owned(),
            ),
            ConfigLayerSource::EnterpriseManaged { id, .. } => {
                (SourceKind::ManagedConfig, format!("cloud:{id}"))
            }
            _ => continue,
        };
        let contents = if id.starts_with("cloud:") {
            layer.raw_toml().map(str::to_owned).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "enterprise managed config has no original source text",
                )
            })?
        } else {
            let Some(bytes) = read_optional(Path::new(&id)).await? else {
                continue;
            };
            String::from_utf8(bytes).map_err(io::Error::other)?
        };
        if !contents.trim().is_empty() {
            sources.push(PolicySource::new(kind, id, contents));
        }
    }
    let path = default_requirements_path()?;
    if let Some(bytes) = read_optional(&path).await? {
        let contents = String::from_utf8(bytes).map_err(io::Error::other)?;
        if !contents.trim().is_empty() {
            sources.push(PolicySource::new(
                SourceKind::ManagedRequirements,
                path.to_string_lossy(),
                contents,
            ));
        }
    }
    Ok(sources)
}

/// Explicit candidate collection; never applies fetched fragments or weakens errors.
pub async fn capture_cloud_managed_sources(
    loader: &CloudConfigBundleLoader,
) -> io::Result<Vec<PolicySource>> {
    let snapshot = loader.get_snapshot().await;
    let mut sources = Vec::new();
    if let Some(bundle) = snapshot.bundle.map_err(io::Error::other)? {
        if !bundle.is_empty() {
            sources.push(PolicySource::new(
                SourceKind::ManagedBundle,
                "cloud:bundle",
                serde_json::to_string(&bundle).map_err(io::Error::other)?,
            ));
        }
        for fragment in bundle.config_toml.enterprise_managed {
            sources.push(PolicySource::new(
                SourceKind::ManagedConfig,
                format!("cloud:{}", fragment.id),
                fragment.contents,
            ));
        }
        for fragment in bundle.requirements_toml.enterprise_managed {
            sources.push(PolicySource::new(
                SourceKind::ManagedRequirements,
                format!("cloud:{}", fragment.id),
                fragment.contents,
            ));
        }
    }
    Ok(sources)
}

impl PinnedManagedFileSystem {
    fn pinned(&self, path: &PathUri) -> Option<io::Result<Vec<u8>>> {
        self.paths.get(path).map(|bytes| {
            bytes.clone().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no accepted managed source at this path",
                )
            })
        })
    }
    fn writable(&self, path: &PathUri) -> io::Result<()> {
        if self.paths.contains_key(path) {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "accepted managed sources are read-only during configuration load",
            ))
        } else {
            Ok(())
        }
    }
}

impl ExecutorFileSystem for PinnedManagedFileSystem {
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        if let Some(bytes) = self.pinned(path) {
            return Box::pin(async move { bytes.map(|_| path.clone()) });
        }
        LOCAL_FS.canonicalize(path, sandbox)
    }
    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        options: ReadFileOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        if let Some(bytes) = self.pinned(path) {
            return Box::pin(async move { bytes });
        }
        LOCAL_FS.read_file(path, options, sandbox)
    }
    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        if let Some(bytes) = self.pinned(path) {
            return Box::pin(async move {
                let _ = bytes?;
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "configuration source streaming is not used during pinned configuration load; use read_file",
                ))
            });
        }
        LOCAL_FS.read_file_stream(path, sandbox)
    }
    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        options: WriteFileOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move {
            self.writable(path)?;
            LOCAL_FS.write_file(path, contents, options, sandbox).await
        })
    }
    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move {
            self.writable(path)?;
            LOCAL_FS.create_directory(path, options, sandbox).await
        })
    }
    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        options: GetMetadataOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        if let Some(bytes) = self.pinned(path) {
            return Box::pin(async move {
                Ok(FileMetadata {
                    is_directory: false,
                    is_file: true,
                    is_symlink: false,
                    size: bytes?.len() as u64,
                    created_at_ms: 0,
                    modified_at_ms: 0,
                })
            });
        }
        LOCAL_FS.get_metadata(path, options, sandbox)
    }
    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        LOCAL_FS.read_directory(path, sandbox)
    }
    fn walk<'a>(
        &'a self,
        path: &'a PathUri,
        options: WalkOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
        LOCAL_FS.walk(path, options, sandbox)
    }
    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        options: RemoveOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move {
            self.writable(path)?;
            LOCAL_FS.remove(path, options, sandbox).await
        })
    }
    fn copy<'a>(
        &'a self,
        source: &'a PathUri,
        destination: &'a PathUri,
        options: CopyOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move {
            self.writable(source)?;
            self.writable(destination)?;
            LOCAL_FS.copy(source, destination, options, sandbox).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_instruction_control::ForkConfig;

    fn snapshot(sources: Vec<PolicySource>) -> PolicySnapshot {
        let mut entries = vec![
            PolicySource::new(SourceKind::MainPrompt, "main", "accepted".into()),
            PolicySource::new(
                SourceKind::ModelCatalog,
                "models",
                r#"{"models":[{"slug":"test","model_messages":{}}]}"#.into(),
            ),
        ];
        entries.extend(sources);
        PolicySnapshot::new(ForkConfig::default(), entries).unwrap()
    }

    #[tokio::test]
    async fn pinned_sources_survive_live_mutation_and_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "live replacement").unwrap();
        let uri = path_uri(&path).unwrap();
        let fs = PinnedManagedFileSystem {
            paths: HashMap::from([(uri.clone(), Some(b"accepted bytes".to_vec()))]),
        };
        assert_eq!(
            fs.read_file(&uri, ReadFileOptions::default(), None)
                .await
                .unwrap(),
            b"accepted bytes"
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            fs.read_file(&uri, ReadFileOptions::default(), None)
                .await
                .unwrap(),
            b"accepted bytes"
        );
        assert!(
            fs.write_file(&uri, b"bad".to_vec(), WriteFileOptions::default(), None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unaccepted_mandatory_requirements_are_not_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let requirements = directory.path().join("requirements.toml");
        std::fs::write(
            &requirements,
            "allowed_approval_policies = [\"on-request\"]",
        )
        .unwrap();
        let overrides = LoaderOverrides {
            system_requirements_path: Some(requirements),
            system_config_path: Some(directory.path().join("config.toml")),
            managed_config_path: Some(directory.path().join("managed_config.toml")),
            ..LoaderOverrides::default()
        };
        let result = prepare(
            &snapshot(Vec::new()),
            directory.path(),
            overrides,
            CloudConfigBundleLoader::default(),
        )
        .await;
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("requires explicit acceptance")
        );
    }

    #[tokio::test]
    async fn accepted_local_auth_constraints_remain_enforced() {
        let directory = tempfile::tempdir().unwrap();
        let requirements = directory.path().join("requirements.toml");
        let content =
            "allowed_login_methods = [\"chatgpt\"]\nallowed_approval_policies = [\"on-request\"]\n";
        std::fs::write(&requirements, content).unwrap();
        let accepted = snapshot(vec![PolicySource::new(
            SourceKind::ManagedRequirements,
            requirements.to_string_lossy(),
            content.into(),
        )]);
        let overrides = LoaderOverrides {
            system_requirements_path: Some(requirements.clone()),
            system_config_path: Some(directory.path().join("config.toml")),
            managed_config_path: Some(directory.path().join("managed_config.toml")),
            ignore_login_requirements: true,
            ignore_managed_requirements: true,
            ..LoaderOverrides::default()
        };
        let (fs, overrides, _) = prepare(
            &accepted,
            directory.path(),
            overrides,
            CloudConfigBundleLoader::default(),
        )
        .await
        .unwrap();
        assert!(!overrides.ignore_login_requirements);
        assert!(!overrides.ignore_managed_requirements);
        let bytes = fs
            .read_file(
                &path_uri(&requirements).unwrap(),
                ReadFileOptions::default(),
                None,
            )
            .await
            .unwrap();
        let values: toml::Value = toml::from_str(std::str::from_utf8(&bytes).unwrap()).unwrap();
        assert_eq!(values["allowed_login_methods"][0].as_str(), Some("chatgpt"));
        assert_eq!(
            values["allowed_approval_policies"][0].as_str(),
            Some("on-request")
        );
    }

    #[tokio::test]
    async fn unaccepted_enterprise_fragment_stops_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let bundle = codex_config::CloudConfigBundle {
            requirements_toml: codex_config::CloudRequirementsTomlBundle {
                enterprise_managed: vec![codex_config::CloudRequirementsFragment {
                    id: "required".into(),
                    name: "Required policy".into(),
                    contents: "allowed_approval_policies = [\"on-request\"]".into(),
                }],
            },
            ..Default::default()
        };
        let overrides = LoaderOverrides {
            system_requirements_path: Some(directory.path().join("requirements.toml")),
            system_config_path: Some(directory.path().join("config.toml")),
            managed_config_path: Some(directory.path().join("managed_config.toml")),
            ..LoaderOverrides::default()
        };
        let result = prepare(
            &snapshot(Vec::new()),
            directory.path(),
            overrides,
            CloudConfigBundleLoader::new(async move { Ok(Some(bundle)) }),
        )
        .await;
        assert!(result.err().unwrap().to_string().contains("cloud:bundle"));
    }

    #[tokio::test]
    async fn reordering_accepted_enterprise_fragments_requires_new_acceptance() {
        let directory = tempfile::tempdir().unwrap();
        let mut bundle = codex_config::CloudConfigBundle {
            requirements_toml: codex_config::CloudRequirementsTomlBundle {
                enterprise_managed: vec![
                    codex_config::CloudRequirementsFragment {
                        id: "first".into(),
                        name: "First".into(),
                        contents: "additional_developer_instructions = \"one\"".into(),
                    },
                    codex_config::CloudRequirementsFragment {
                        id: "second".into(),
                        name: "Second".into(),
                        contents: "additional_developer_instructions = \"two\"".into(),
                    },
                ],
            },
            ..Default::default()
        };
        let sources = capture_cloud_managed_sources(&CloudConfigBundleLoader::new({
            let bundle = bundle.clone();
            async move { Ok(Some(bundle)) }
        }))
        .await
        .unwrap();
        let initial = snapshot(sources);
        let mut config = initial.config;
        config.controls.managed_requirements = ManagedRequirements::ApprovedSources;
        let accepted = PolicySnapshot::new(config, initial.sources).unwrap();
        let overrides = || LoaderOverrides {
            system_requirements_path: Some(directory.path().join("requirements.toml")),
            system_config_path: Some(directory.path().join("config.toml")),
            managed_config_path: Some(directory.path().join("managed_config.toml")),
            ..LoaderOverrides::default()
        };
        let original_loader = CloudConfigBundleLoader::new({
            let bundle = bundle.clone();
            async move { Ok(Some(bundle)) }
        });
        assert!(
            prepare(&accepted, directory.path(), overrides(), original_loader)
                .await
                .is_ok()
        );
        bundle.requirements_toml.enterprise_managed.reverse();
        let reordered = prepare(
            &accepted,
            directory.path(),
            overrides(),
            CloudConfigBundleLoader::new(async move { Ok(Some(bundle)) }),
        )
        .await;
        assert!(
            reordered
                .err()
                .unwrap()
                .to_string()
                .contains("ordered fragment manifest")
        );
    }
}
