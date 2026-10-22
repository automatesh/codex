use super::Config;
use super::TokenBudgetConfig;
use codex_features::Feature;
use codex_instruction_control::PolicySource;
use codex_instruction_control::SourceKind;
use serde::Deserialize;
use serde::Serialize;
use std::io;
use std::num::NonZeroUsize;

const SETTINGS_ID: &str = "instruction-settings";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstructionSettings {
    schema_version: u32,
    include_permissions_instructions: bool,
    include_apps_instructions: bool,
    include_collaboration_mode_instructions: bool,
    include_skill_instructions: bool,
    skill_max_context_tokens: Option<NonZeroUsize>,
    project_doc_max_bytes: usize,
    project_doc_fallback_filenames: Vec<String>,
    compact_prompt: Option<String>,
    realtime_backend_prompt: Option<String>,
    realtime_startup_context: Option<String>,
    realtime_start_instructions: Option<String>,
    token_budget_enabled: bool,
    token_budget: Option<TokenBudgetConfig>,
    codex_hooks_enabled: bool,
    plugin_hooks_enabled: bool,
    plugins_enabled: bool,
    remote_plugins_enabled: bool,
    plugin_default_enabled: Option<bool>,
    plugin_enabled: std::collections::BTreeMap<String, Option<bool>>,
    multi_agent_usage_hint: Option<String>,
    multi_agent_root_hint: Option<String>,
    multi_agent_subagent_hint: Option<String>,
    multi_agent_subagent_instructions: Option<String>,
    multi_agent_mode_hint: Option<String>,
}

impl From<&Config> for InstructionSettings {
    fn from(config: &Config) -> Self {
        Self {
            schema_version: 1,
            include_permissions_instructions: config.include_permissions_instructions,
            include_apps_instructions: config.include_apps_instructions,
            include_collaboration_mode_instructions: config.include_collaboration_mode_instructions,
            include_skill_instructions: config.include_skill_instructions,
            skill_max_context_tokens: config.skill_max_context_tokens,
            project_doc_max_bytes: config.project_doc_max_bytes,
            project_doc_fallback_filenames: config.project_doc_fallback_filenames.clone(),
            compact_prompt: config.compact_prompt.clone(),
            realtime_backend_prompt: config.experimental_realtime_ws_backend_prompt.clone(),
            realtime_startup_context: config.experimental_realtime_ws_startup_context.clone(),
            realtime_start_instructions: config.experimental_realtime_start_instructions.clone(),
            token_budget_enabled: config.features.enabled(Feature::TokenBudget),
            token_budget: config.token_budget.clone(),
            codex_hooks_enabled: config.features.enabled(Feature::CodexHooks),
            plugin_hooks_enabled: config.features.enabled(Feature::PluginHooks),
            plugins_enabled: config.features.enabled(Feature::Plugins),
            remote_plugins_enabled: config.features.enabled(Feature::RemotePlugin),
            plugin_default_enabled: config
                .plugins
                .default
                .as_ref()
                .map(|default| default.enabled),
            plugin_enabled: config
                .plugins
                .plugins
                .iter()
                .map(|(id, plugin)| (id.clone(), plugin.enabled))
                .collect(),
            multi_agent_usage_hint: config.multi_agent_v2.usage_hint_text.clone(),
            multi_agent_root_hint: config.multi_agent_v2.root_agent_usage_hint_text.clone(),
            multi_agent_subagent_hint: config.multi_agent_v2.subagent_usage_hint_text.clone(),
            multi_agent_subagent_instructions: config
                .multi_agent_v2
                .subagent_developer_instructions
                .clone(),
            multi_agent_mode_hint: config.multi_agent_v2.multi_agent_mode_hint_text.clone(),
        }
    }
}

pub fn capture_instruction_settings(config: &Config) -> io::Result<PolicySource> {
    Ok(PolicySource::new(
        SourceKind::Builtin,
        SETTINGS_ID,
        serde_json::to_string_pretty(&InstructionSettings::from(config))
            .map_err(io::Error::other)?,
    ))
}

pub(super) fn apply_instruction_settings(config: &mut Config) -> io::Result<()> {
    let Some(snapshot) = &config.instruction_snapshot else {
        return Ok(());
    };
    let global_home = snapshot.resolve(SourceKind::Builtin, "global-agents-home").map_err(|error| {
        io::Error::new(io::ErrorKind::PermissionDenied,
            format!("Accepted global instruction home is missing: {error}; run codex instructions capture and approve the complete candidate"))
    })?;
    codex_utils_absolute_path::AbsolutePathBuf::try_from(std::path::PathBuf::from(global_home))
        .map_err(io::Error::other)?;
    let raw = snapshot.resolve(SourceKind::Builtin, SETTINGS_ID).map_err(|error| {
        io::Error::new(io::ErrorKind::PermissionDenied,
            format!("Accepted instruction selection is missing: {error}; run codex instructions capture, review and approve the complete candidate"))
    })?;
    let accepted: InstructionSettings = serde_json::from_str(raw).map_err(io::Error::other)?;
    if accepted.schema_version != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported accepted instruction-settings schema",
        ));
    }
    if let Some(budget) = &accepted.token_budget {
        budget.validate()?;
    }
    config
        .features
        .set_enabled(Feature::TokenBudget, accepted.token_budget_enabled)
        .map_err(io::Error::other)?;
    config
        .features
        .set_enabled(Feature::CodexHooks, accepted.codex_hooks_enabled)
        .map_err(io::Error::other)?;
    config
        .features
        .set_enabled(Feature::PluginHooks, accepted.plugin_hooks_enabled)
        .map_err(io::Error::other)?;
    config
        .features
        .set_enabled(Feature::Plugins, accepted.plugins_enabled)
        .map_err(io::Error::other)?;
    config
        .features
        .set_enabled(Feature::RemotePlugin, accepted.remote_plugins_enabled)
        .map_err(io::Error::other)?;
    config.plugins.default = accepted
        .plugin_default_enabled
        .map(|enabled| codex_config::types::PluginsDefaultConfig { enabled });
    for (id, plugin) in &mut config.plugins.plugins {
        plugin.enabled = accepted.plugin_enabled.get(id).copied().flatten();
    }
    for (id, enabled) in accepted.plugin_enabled {
        config
            .plugins
            .plugins
            .entry(id)
            .or_insert_with(|| codex_config::types::PluginConfig {
                enabled,
                mcp_servers: Default::default(),
            });
    }
    config.include_permissions_instructions = accepted.include_permissions_instructions;
    config.include_apps_instructions = accepted.include_apps_instructions;
    config.include_collaboration_mode_instructions =
        accepted.include_collaboration_mode_instructions;
    config.include_skill_instructions = accepted.include_skill_instructions;
    config.skill_max_context_tokens = accepted.skill_max_context_tokens;
    config.project_doc_max_bytes = accepted.project_doc_max_bytes;
    config.project_doc_fallback_filenames = accepted.project_doc_fallback_filenames;
    config.compact_prompt = accepted.compact_prompt;
    config.experimental_realtime_ws_backend_prompt = accepted.realtime_backend_prompt;
    config.experimental_realtime_ws_startup_context = accepted.realtime_startup_context;
    config.experimental_realtime_start_instructions = accepted.realtime_start_instructions;
    config.token_budget = accepted.token_budget;
    config.token_budget_startup_config = None;
    config.multi_agent_v2.usage_hint_text = accepted.multi_agent_usage_hint;
    config.multi_agent_v2.root_agent_usage_hint_text = accepted.multi_agent_root_hint;
    config.multi_agent_v2.subagent_usage_hint_text = accepted.multi_agent_subagent_hint;
    config.multi_agent_v2.subagent_developer_instructions =
        accepted.multi_agent_subagent_instructions;
    config.multi_agent_v2.multi_agent_mode_hint_text = accepted.multi_agent_mode_hint;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_instruction_control::PolicySnapshot;
    use std::sync::Arc;

    async fn config() -> (tempfile::TempDir, Config) {
        let directory = tempfile::tempdir().unwrap();
        let config = super::super::ConfigBuilder::default()
            .codex_home(directory.path().to_path_buf())
            .harness_overrides(super::super::ConfigOverrides {
                cwd: Some(directory.path().to_path_buf()),
                ephemeral: Some(true),
                ..Default::default()
            })
            .loader_overrides(codex_config::LoaderOverrides::without_managed_config_for_tests())
            .build()
            .await
            .unwrap();
        (directory, config)
    }

    fn snapshot(settings: Option<PolicySource>) -> Arc<PolicySnapshot> {
        let mut sources = vec![
            PolicySource::new(
                SourceKind::Builtin,
                "global-agents-home",
                std::env::current_dir()
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
            ),
            PolicySource::new(SourceKind::MainPrompt, "main", "main prompt".to_string()),
            PolicySource::new(
                SourceKind::ModelCatalog,
                "models",
                r#"{"models":[{"slug":"test","model_messages":{}}]}"#.to_string(),
            ),
        ];
        sources.extend(settings);
        Arc::new(PolicySnapshot::new(Default::default(), sources).unwrap())
    }

    #[tokio::test]
    async fn accepted_selection_cannot_be_removed_by_live_config_without_freezing_workdir_or_permissions()
     {
        let (_directory, mut config) = config().await;
        let original_limit = config.project_doc_max_bytes;
        let original_visibility = config.include_skill_instructions;
        let original_cwd = config.cwd.clone();
        let original_permission = config.permissions.permission_profile().clone();
        config.plugins.default = Some(codex_config::types::PluginsDefaultConfig { enabled: true });
        config.plugins.plugins.insert(
            "accepted-plugin".to_string(),
            codex_config::types::PluginConfig {
                enabled: Some(true),
                mcp_servers: Default::default(),
            },
        );
        config.instruction_snapshot = Some(snapshot(Some(
            capture_instruction_settings(&config).unwrap(),
        )));
        config.project_doc_max_bytes = 0;
        config.include_skill_instructions = !original_visibility;
        config.plugins.default = Some(codex_config::types::PluginsDefaultConfig { enabled: false });
        config
            .plugins
            .plugins
            .get_mut("accepted-plugin")
            .unwrap()
            .enabled = Some(false);
        config.compact_prompt = Some("unapproved replacement".to_string());
        config.experimental_realtime_ws_backend_prompt =
            Some("unapproved realtime prompt".to_string());
        apply_instruction_settings(&mut config).unwrap();
        assert_eq!(config.project_doc_max_bytes, original_limit);
        assert_eq!(config.include_skill_instructions, original_visibility);
        assert!(config.plugins.default.as_ref().unwrap().enabled);
        assert_eq!(
            config.plugins.plugins["accepted-plugin"].enabled,
            Some(true)
        );
        assert_ne!(
            config.compact_prompt.as_deref(),
            Some("unapproved replacement")
        );
        assert_ne!(
            config.experimental_realtime_ws_backend_prompt.as_deref(),
            Some("unapproved realtime prompt")
        );
        assert_eq!(config.cwd, original_cwd);
        assert_eq!(
            config.permissions.permission_profile(),
            &original_permission
        );
    }

    #[tokio::test]
    async fn missing_accepted_selection_is_an_explicit_migration_error() {
        let (_directory, mut config) = config().await;
        config.instruction_snapshot = Some(snapshot(None));
        let error = apply_instruction_settings(&mut config).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("capture"));
    }
}
