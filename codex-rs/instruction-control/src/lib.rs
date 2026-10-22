use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use base64::Engine;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::path::PathBuf;

pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

/// Import only model metadata, never the cache's identity or refresh bookkeeping.
pub fn normalize_model_catalog(raw: &str) -> Result<String> {
    let raw: serde_json::Value = serde_json::from_str(raw).context("invalid model catalog JSON")?;
    let models = raw
        .get("models")
        .filter(|value| value.is_array())
        .context("model catalog models array missing")?;
    Ok(serde_json::to_string_pretty(
        &serde_json::json!({ "models": models }),
    )?)
}

pub mod runtime;
pub use runtime::initialize_runtime;
pub use runtime::runtime_snapshot;

#[cfg(windows)]
pub mod windows;

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ForkConfig {
    pub instructions: InstructionConfig,
    pub controls: ControlsConfig,
    pub git_attribution: GitAttributionConfig,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct InstructionConfig {
    pub updates: InstructionUpdates,
    pub resume: ResumeInstructions,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum InstructionUpdates {
    #[default]
    Manual,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ResumeInstructions {
    #[default]
    Rebuild,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ControlsConfig {
    pub dangerous_commands: DangerousCommands,
    pub managed_requirements: ManagedRequirements,
    pub hooks: HookControl,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DangerousCommands {
    #[default]
    Confirm,
    Warn,
    Block,
    Off,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedRequirements {
    #[default]
    LocalOnly,
    ApprovedSources,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HookControl {
    #[default]
    ApprovedOnly,
    Off,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct GitAttributionConfig {
    pub commit_trailer: String,
    pub pr_footer: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    MainPrompt,
    ModelCatalog,
    DeveloperInstructions,
    Agents,
    Skill,
    Plugin,
    ToolDescription,
    ConfirmationPolicy,
    Hook,
    ManagedRequirements,
    ManagedConfig,
    ManagedBundle,
    Builtin,
    InstructionFragment,
    ToolSchema,
    Asset,
    SkillAsset,
    SkillMetadata,
    PluginAsset,
    HookAsset,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ContentEncoding {
    #[default]
    Utf8,
    Base64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PolicySource {
    pub kind: SourceKind,
    pub logical_id: String,
    pub sha256: String,
    pub content: String,
    #[serde(default)]
    pub encoding: ContentEncoding,
}

impl PolicySource {
    pub fn new(kind: SourceKind, logical_id: impl Into<String>, content: String) -> Self {
        Self {
            kind,
            logical_id: logical_id.into(),
            sha256: sha256(content.as_bytes()),
            content,
            encoding: ContentEncoding::Utf8,
        }
    }

    pub fn asset(kind: SourceKind, logical_id: impl Into<String>, bytes: Vec<u8>) -> Self {
        let sha256 = sha256(&bytes);
        match String::from_utf8(bytes) {
            Ok(content) if !content.contains('\0') => Self {
                kind,
                logical_id: logical_id.into(),
                sha256,
                content,
                encoding: ContentEncoding::Utf8,
            },
            result => {
                let bytes = match result {
                    Ok(content) => content.into_bytes(),
                    Err(error) => error.into_bytes(),
                };
                Self {
                    kind,
                    logical_id: logical_id.into(),
                    sha256,
                    content: base64::engine::general_purpose::STANDARD.encode(bytes),
                    encoding: ContentEncoding::Base64,
                }
            }
        }
    }

    pub fn bytes(&self) -> Result<Cow<'_, [u8]>> {
        match self.encoding {
            ContentEncoding::Utf8 => Ok(Cow::Borrowed(self.content.as_bytes())),
            ContentEncoding::Base64 => Ok(Cow::Owned(
                base64::engine::general_purpose::STANDARD.decode(&self.content)?,
            )),
        }
    }

    /// Absolute original source paths retain package layout under a protected tree.
    /// Non-path identifiers use a content-addressed file instead.
    pub fn relative_copy_path(&self) -> Result<PathBuf> {
        if self.kind == SourceKind::SkillMetadata {
            return Ok(PathBuf::from("metadata").join(format!("{}.json", self.sha256)));
        }
        let id = self.logical_id.replace('\\', "/");
        let absolute_drive = id.as_bytes().get(1) == Some(&b':')
            && id.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && id.as_bytes().get(2) == Some(&b'/');
        if absolute_drive || id.starts_with('/') {
            let mut result = PathBuf::from("tree");
            let remainder = if absolute_drive {
                result.push(id[..1].to_ascii_uppercase());
                &id[3..]
            } else {
                if id.starts_with("//") {
                    bail!("UNC instruction source paths are not supported");
                }
                result.push("root");
                &id[1..]
            };
            for component in remainder.split('/') {
                if component.is_empty()
                    || component == "."
                    || component == ".."
                    || component.ends_with(['.', ' '])
                    || component.chars().any(|c| {
                        c.is_control() || matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*')
                    })
                {
                    bail!("unsafe instruction source path: {}", self.logical_id);
                }
                let stem = component
                    .split('.')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_uppercase();
                if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                    || (stem.len() == 4
                        && (stem.starts_with("COM") || stem.starts_with("LPT"))
                        && stem.as_bytes()[3].is_ascii_digit())
                {
                    bail!(
                        "reserved Windows instruction source path: {}",
                        self.logical_id
                    );
                }
                result.push(component);
            }
            if self.kind == SourceKind::Hook {
                let original = result
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("hook source filename missing")?;
                result.set_file_name(format!(".accepted-hooks-{original}.json"));
            }
            return Ok(result);
        }
        Ok(PathBuf::from(format!("{}.txt", self.sha256)))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PolicySnapshot {
    pub schema_version: u32,
    pub snapshot_id: String,
    pub config: ForkConfig,
    pub sources: Vec<PolicySource>,
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl PolicySnapshot {
    pub fn new(config: ForkConfig, sources: Vec<PolicySource>) -> Result<Self> {
        let mut snapshot = Self {
            schema_version: 1,
            snapshot_id: String::new(),
            config,
            sources,
        };
        snapshot
            .sources
            .sort_by(|a, b| (&a.kind, &a.logical_id).cmp(&(&b.kind, &b.logical_id)));
        snapshot.snapshot_id = snapshot.content_id()?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    fn content_id(&self) -> Result<String> {
        Ok(sha256(&serde_json::to_vec(&(
            self.schema_version,
            &self.config,
            &self.sources,
        ))?))
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            bail!("instruction snapshot exceeds 64 MiB");
        }
        let snapshot: Self =
            serde_json::from_slice(bytes).context("invalid instruction snapshot")?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 || self.snapshot_id != self.content_id()? {
            bail!("instruction snapshot version or digest mismatch");
        }
        let mut seen = BTreeSet::new();
        let mut paths = std::collections::BTreeMap::new();
        for source in &self.sources {
            if source.logical_id.is_empty() || source.logical_id.chars().any(char::is_control) {
                bail!("empty or invalid instruction source identifier");
            }
            if source.encoding == ContentEncoding::Utf8 && source.content.contains('\0') {
                bail!("instruction source contains NUL: {}", source.logical_id);
            }
            if !seen.insert((&source.kind, &source.logical_id)) {
                bail!(
                    "duplicate instruction source {:?}/{}",
                    source.kind,
                    source.logical_id
                );
            }
            if source.sha256 != sha256(&source.bytes()?) {
                bail!("instruction source digest mismatch: {}", source.logical_id);
            }
            let path = source
                .relative_copy_path()?
                .to_string_lossy()
                .to_lowercase();
            if let Some(existing) = paths.insert(path, &source.sha256) {
                if existing != &source.sha256 {
                    bail!("instruction source paths collide: {}", source.logical_id);
                }
            }
        }
        for kind in [SourceKind::MainPrompt, SourceKind::ModelCatalog] {
            if self
                .sources
                .iter()
                .filter(|source| source.kind == kind)
                .count()
                != 1
            {
                bail!("snapshot must contain exactly one {kind:?}");
            }
            if self
                .sources
                .iter()
                .any(|source| source.kind == kind && source.encoding != ContentEncoding::Utf8)
            {
                bail!("instruction source {kind:?} must be UTF-8 text");
            }
        }
        let catalog = self
            .sources
            .iter()
            .find(|source| source.kind == SourceKind::ModelCatalog)
            .context("model catalog missing")?;
        let catalog: serde_json::Value =
            serde_json::from_str(&catalog.content).context("model catalog must be valid JSON")?;
        let models = catalog
            .get("models")
            .and_then(serde_json::Value::as_array)
            .filter(|models| !models.is_empty())
            .context("model catalog must contain a nonempty models array")?;
        let mut slugs = BTreeSet::new();
        for model in models {
            let slug = model
                .get("slug")
                .and_then(serde_json::Value::as_str)
                .filter(|slug| !slug.is_empty())
                .context("catalog model slug missing")?;
            if !slugs.insert(slug) {
                bail!("duplicate model slug in accepted catalog: {slug}");
            }
            if !model
                .get("model_messages")
                .is_some_and(serde_json::Value::is_object)
            {
                bail!("catalog model {slug} is missing model_messages object");
            }
        }
        Ok(())
    }

    pub fn resolve(&self, kind: SourceKind, logical_id: &str) -> Result<&str> {
        self.sources.iter().find(|source| source.kind == kind && source.logical_id == logical_id && source.encoding == ContentEncoding::Utf8)
            .map(|source| source.content.as_str())
            .with_context(|| format!("instruction source not accepted: {kind:?}/{logical_id}; prepare a candidate and approve it with codex-policy-manager"))
    }

    pub fn single(&self, kind: SourceKind) -> Result<&str> {
        self.sources
            .iter()
            .find(|source| source.kind == kind && source.encoding == ContentEncoding::Utf8)
            .map(|source| source.content.as_str())
            .with_context(|| format!("instruction source not accepted: {kind:?}"))
    }

    pub fn verify_source(&self, kind: SourceKind, logical_id: &str, bytes: &[u8]) -> Result<()> {
        let accepted = self.resolve(kind, logical_id)?;
        if accepted.as_bytes() != bytes {
            bail!("instruction source changed: {logical_id}; accepted snapshot remains active");
        }
        Ok(())
    }

    pub fn verify_content(&self, kind: SourceKind, logical_id: &str, content: &str) -> Result<()> {
        self.verify_source(kind, logical_id, content.as_bytes())
    }

    pub fn verify_bytes(&self, bytes: &[u8]) -> Result<()> {
        if !self.sources.iter().any(|source| {
            source
                .bytes()
                .is_ok_and(|content| content.as_ref() == bytes)
        }) {
            bail!(
                "instruction content not present in accepted snapshot {}",
                self.snapshot_id
            );
        }
        Ok(())
    }

    pub fn source_path(&self, kind: SourceKind, logical_id: &str) -> Result<PathBuf> {
        #[cfg(windows)]
        {
            windows::source_path(self, kind, logical_id)
        }
        #[cfg(not(windows))]
        {
            let _ = (kind, logical_id);
            bail!("protected instruction source copies require Windows")
        }
    }

    pub fn load_protected() -> Result<Self> {
        #[cfg(windows)]
        {
            windows::load_protected()
        }
        #[cfg(not(windows))]
        {
            bail!(
                "protected instruction storage requires Windows; no unprotected fallback is permitted"
            )
        }
    }
}

pub fn protected_install_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        windows::install_dir()
    }
    #[cfg(not(windows))]
    {
        bail!("protected instruction storage requires Windows")
    }
}

pub fn review_text(old: Option<&PolicySnapshot>, candidate: &PolicySnapshot) -> Result<String> {
    let mut text = format!(
        "Previous snapshot: {}\r\nCandidate snapshot: {}\r\n\r\n",
        old.map_or("NONE (first installation)", |s| s.snapshot_id.as_str()),
        candidate.snapshot_id
    );
    text.push_str("=== PREVIOUS CONTROLS ===\r\n");
    text.push_str(
        &old.map(|s| serde_json::to_string_pretty(&s.config))
            .transpose()?
            .unwrap_or_else(|| "NONE".into()),
    );
    text.push_str("\r\n=== CANDIDATE CONTROLS ===\r\n");
    text.push_str(&serde_json::to_string_pretty(&candidate.config)?);
    let mut keys = BTreeSet::new();
    for source in old
        .into_iter()
        .flat_map(|s| &s.sources)
        .chain(&candidate.sources)
    {
        keys.insert((source.kind.clone(), source.logical_id.clone()));
    }
    for (kind, id) in keys {
        let before = old.and_then(|s| {
            s.sources
                .iter()
                .find(|entry| entry.kind == kind && entry.logical_id == id)
        });
        let after = candidate
            .sources
            .iter()
            .find(|entry| entry.kind == kind && entry.logical_id == id);
        if before == after {
            continue;
        }
        let render = |entry: Option<&PolicySource>, absent: &str| {
            entry.map_or_else(
                || absent.into(),
                |source| {
                    format!(
                        "Encoding: {:?}\r\nSHA-256: {}\r\n{}",
                        source.encoding, source.sha256, source.content
                    )
                },
            )
        };
        text.push_str(&format!("\r\n\r\n=== {kind:?}: {id} ===\r\n--- PREVIOUS EXACT CONTENT ---\r\n{}\r\n+++ CANDIDATE EXACT CONTENT +++\r\n{}", render(before, "[ABSENT]"), render(after, "[REMOVED]")));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> PolicySnapshot {
        PolicySnapshot::new(
            ForkConfig::default(),
            vec![
                PolicySource::new(
                    SourceKind::MainPrompt,
                    "main",
                    "original instructions".into(),
                ),
                PolicySource::new(
                    SourceKind::ModelCatalog,
                    "models",
                    "{\"models\":[{\"slug\":\"test\",\"model_messages\":{}}]}".into(),
                ),
            ],
        )
        .unwrap()
    }

    #[test]
    fn detects_source_and_control_tampering() {
        let original = snapshot();
        let mut modified = original.clone();
        modified.sources[0].content.push('!');
        assert!(modified.validate().is_err());
        let mut modified = original;
        modified.config.controls.dangerous_commands = DangerousCommands::Off;
        assert!(modified.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_source_and_unknown_fields() {
        let mut sources = snapshot().sources;
        sources.push(sources[0].clone());
        assert!(PolicySnapshot::new(ForkConfig::default(), sources).is_err());
        assert!(serde_json::from_str::<ForkConfig>("{\"surprise\":true}").is_err());
    }

    #[test]
    fn accepted_content_does_not_follow_source_changes() {
        let snapshot = snapshot();
        assert_eq!(
            snapshot.resolve(SourceKind::MainPrompt, "main").unwrap(),
            "original instructions"
        );
        assert!(
            snapshot
                .verify_source(SourceKind::MainPrompt, "main", b"updated instructions")
                .is_err()
        );
        assert!(snapshot.resolve(SourceKind::Hook, "unknown").is_err());
        assert!(snapshot.verify_bytes(b"unaccepted").is_err());
    }

    #[test]
    fn review_contains_deleted_and_replacement_text() {
        let original = snapshot();
        let next = PolicySnapshot::new(
            ForkConfig::default(),
            vec![
                PolicySource::new(
                    SourceKind::MainPrompt,
                    "main",
                    "replacement instructions".into(),
                ),
                PolicySource::new(
                    SourceKind::ModelCatalog,
                    "models",
                    "{\"models\":[{\"slug\":\"test\",\"model_messages\":{}}]}".into(),
                ),
            ],
        )
        .unwrap();
        let review = review_text(Some(&original), &next).unwrap();
        assert!(review.contains("original instructions"));
        assert!(review.contains("replacement instructions"));
        assert!(review.contains(&next.snapshot_id));
    }

    #[test]
    fn source_paths_preserve_package_layout_and_reject_traversal() {
        let source = PolicySource::asset(
            SourceKind::SkillAsset,
            "C:/Users/test/skill/scripts/run.py",
            b"print('ok')".to_vec(),
        );
        assert_eq!(
            source.relative_copy_path().unwrap(),
            PathBuf::from("tree")
                .join("C")
                .join("Users/test/skill/scripts/run.py")
        );
        for path in [
            "C:/skills/../escape",
            "C:/skills/CON.txt",
            "C:/skills/file:stream",
            "C:/skills/dir./x",
            "//server/share/x",
        ] {
            assert!(
                PolicySource::asset(SourceKind::SkillAsset, path, vec![0])
                    .relative_copy_path()
                    .is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn binary_asset_round_trips_and_is_shown_in_review() {
        let asset = PolicySource::asset(
            SourceKind::SkillAsset,
            "C:/skill/image.png",
            vec![0, 255, 1],
        );
        assert_eq!(asset.bytes().unwrap().as_ref(), &[0, 255, 1]);
        let original = snapshot();
        let mut entries = original.sources.clone();
        entries.push(asset.clone());
        let changed = PolicySnapshot::new(ForkConfig::default(), entries).unwrap();
        let parsed = PolicySnapshot::parse(&serde_json::to_vec(&changed).unwrap()).unwrap();
        assert_eq!(parsed, changed);
        assert!(
            review_text(Some(&original), &changed)
                .unwrap()
                .contains(&asset.sha256)
        );
    }

    #[test]
    fn rejects_empty_catalogs_and_case_colliding_package_paths() {
        let original = snapshot();
        for content in [
            "{}",
            "{\"models\":[]}",
            "{\"models\":[{\"slug\":\"test\"}]}",
        ] {
            let mut entries = original.sources.clone();
            entries.retain(|source| source.kind != SourceKind::ModelCatalog);
            entries.push(PolicySource::new(
                SourceKind::ModelCatalog,
                "models",
                content.into(),
            ));
            assert!(PolicySnapshot::new(ForkConfig::default(), entries).is_err());
        }
        let mut entries = original.sources;
        entries.push(PolicySource::asset(
            SourceKind::SkillAsset,
            "C:/Skill/FILE.txt",
            b"one".to_vec(),
        ));
        entries.push(PolicySource::asset(
            SourceKind::SkillAsset,
            "c:/skill/file.TXT",
            b"two".to_vec(),
        ));
        assert!(PolicySnapshot::new(ForkConfig::default(), entries).is_err());
    }

    #[test]
    fn skill_metadata_does_not_replace_skill_document() {
        let skill = PolicySource::new(
            SourceKind::Skill,
            "C:/skill/SKILL.md",
            "instructions".into(),
        );
        let metadata = PolicySource::new(
            SourceKind::SkillMetadata,
            "C:/skill/SKILL.md",
            "{\"name\":\"skill\"}".into(),
        );
        assert_ne!(
            skill.relative_copy_path().unwrap(),
            metadata.relative_copy_path().unwrap()
        );
    }

    #[test]
    fn model_cache_import_removes_identity_but_preserves_model_metadata() {
        let normalized = normalize_model_catalog(r#"{"identity":{"account":"private"},"fetched_at":"yesterday","etag":"abc","models":[{"slug":"test","model_messages":{"instructions_template":"keep exact"}}]}"#).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&normalized).unwrap();
        assert_eq!(parsed.as_object().unwrap().len(), 1);
        assert_eq!(
            parsed["models"][0]["model_messages"]["instructions_template"],
            "keep exact"
        );
        assert!(!normalized.contains("private"));
    }

    #[test]
    fn oversized_candidates_are_rejected_before_parsing() {
        assert!(
            PolicySnapshot::parse(&vec![b' '; MAX_SNAPSHOT_BYTES + 1])
                .unwrap_err()
                .to_string()
                .contains("64 MiB")
        );
    }
}
