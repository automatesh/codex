use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_features::Feature;
use codex_instruction_control::PolicySnapshot;
use codex_instruction_control::PolicySource;
use codex_instruction_control::SourceKind;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, clap::Args)]
pub(crate) struct InstructionsCommand {
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<OsString>,
}

#[derive(Debug, Parser)]
#[command(
    name = "codex instructions capture",
    about = "Capture current instruction sources for explicit candidate review without generating a model response"
)]
struct CaptureArgs {
    #[arg(long)]
    candidate: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    model: Option<String>,
    /// Fetch cloud-managed source candidates explicitly, without applying them.
    #[arg(long)]
    include_cloud: bool,
}

pub(crate) async fn run(command: InstructionsCommand) -> Result<()> {
    if command
        .args
        .first()
        .is_some_and(|argument| argument == "capture")
    {
        let args = CaptureArgs::try_parse_from(
            std::iter::once(OsString::from("codex instructions capture"))
                .chain(command.args.into_iter().skip(1)),
        )?;
        return capture(args).await;
    }
    let manager = std::env::current_exe()?.with_file_name(if cfg!(windows) {
        "codex-policy-manager.exe"
    } else {
        "codex-policy-manager"
    });
    let status = std::process::Command::new(&manager)
        .args(command.args)
        .status()
        .with_context(|| format!("starting {}", manager.display()))?;
    if !status.success() {
        bail!("instruction manager exited with {status}");
    }
    Ok(())
}

async fn capture(args: CaptureArgs) -> Result<()> {
    let candidate = PolicySnapshot::parse(
        &std::fs::read(&args.candidate)
            .with_context(|| format!("reading candidate {}", args.candidate.display()))?,
    )?;
    if args.output.exists() {
        bail!(
            "output already exists: {}; choose a new candidate path",
            args.output.display()
        );
    }
    let mut config = ConfigBuilder::default()
        .harness_overrides(ConfigOverrides {
            cwd: args.cwd,
            model: args.model,
            ephemeral: Some(true),
            codex_self_exe: Some(std::env::current_exe()?),
            ..Default::default()
        })
        .build()
        .await?;
    config.instruction_snapshot = None;
    config.fork = candidate.config.clone();
    config.base_instructions = Some(candidate.single(SourceKind::MainPrompt)?.to_string());
    config.model_catalog = Some(serde_json::from_str(
        candidate.single(SourceKind::ModelCatalog)?,
    )?);
    let developer = candidate
        .sources
        .iter()
        .filter(|source| source.kind == SourceKind::DeveloperInstructions)
        .map(|source| source.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    config.developer_instructions = (!developer.is_empty()).then_some(developer);
    config.ephemeral = true;
    config.cloud_skill_enabled = false;
    let instruction_settings = codex_core::config::capture_instruction_settings(&config)?;
    for feature in [
        Feature::CodexHooks,
        Feature::PluginHooks,
        Feature::CodeModePrewarm,
        Feature::ResponsesWebsockets,
        Feature::ResponsesWebsocketsV2,
    ] {
        config.features.disable(feature).map_err(|error| {
            anyhow::anyhow!("cannot disable {feature:?} during instruction capture: {error}")
        })?;
    }

    let auth = codex_login::AuthManager::shared_from_config(&config, false).await?;
    let plugins_manager = codex_core::plugins_manager_for_config(&config, auth);
    let mut plugin_input = config.plugins_config_input();
    plugin_input.remote_plugin_enabled = false;
    let plugins = plugins_manager.plugins_for_config(&plugin_input).await;
    let mut plugin_diagnostics = Vec::new();
    for plugin in plugins
        .plugins()
        .iter()
        .filter(|plugin| plugin.enabled && plugin.error.is_some())
    {
        let error = plugin.error.as_deref().unwrap_or_default();
        eprintln!(
            "Warning: enabled plugin {} at {} is unavailable and excluded from the effective capture inventory: {}",
            plugin.config_name,
            plugin.root.display(),
            error,
        );
        plugin_diagnostics.push(serde_json::json!({
            "plugin": plugin.config_name,
            "root": plugin.root.to_string_lossy(),
            "error": error,
        }));
    }
    plugin_diagnostics.sort_by_cached_key(serde_json::Value::to_string);
    let service = codex_skills_extension::HostSkillsService::new(
        config.codex_home.clone(),
        config.bundled_skills_enabled(),
    );
    let skill_input = codex_skills_extension::HostSkillsLoadInput::new(
        config.cwd.clone(),
        plugins.effective_plugin_skill_roots(),
        config.config_layer_stack.clone(),
    );
    let mut sources = candidate
        .sources
        .into_iter()
        .filter(|source| {
            if source.kind == SourceKind::Builtin
                && matches!(
                    source.logical_id.as_str(),
                    "instruction-settings" | "global-agents-home" | "capture-plugin-diagnostics"
                )
            {
                return false;
            }
            if matches!(
                source.kind,
                SourceKind::ManagedConfig
                    | SourceKind::ManagedRequirements
                    | SourceKind::ManagedBundle
            ) {
                return source.logical_id.starts_with("cloud:") && !args.include_cloud;
            }
            !matches!(
                source.kind,
                SourceKind::Skill
                    | SourceKind::SkillMetadata
                    | SourceKind::SkillAsset
                    | SourceKind::Agents
                    | SourceKind::Hook
                    | SourceKind::HookAsset
                    | SourceKind::InstructionFragment
                    | SourceKind::ToolSchema
            )
        })
        .map(|source| ((source.kind.clone(), source.logical_id.clone()), source))
        .collect::<BTreeMap<_, _>>();
    extend_sources(&mut sources, vec![instruction_settings])?;
    if !plugin_diagnostics.is_empty() {
        extend_sources(
            &mut sources,
            vec![PolicySource::new(
                SourceKind::Builtin,
                "capture-plugin-diagnostics",
                serde_json::to_string_pretty(&serde_json::json!({
                    "schema_version": 1,
                    "inventory": "effective_available_plugins",
                    "excluded_unavailable_plugins": plugin_diagnostics,
                }))?,
            )],
        )?;
    }
    extend_sources(
        &mut sources,
        codex_skills_extension::capture_instruction_sources(&service, &skill_input)
            .await
            .map_err(anyhow::Error::msg)?,
    )?;
    extend_sources(
        &mut sources,
        codex_core::capture_agents_sources(&config).await?,
    )?;
    extend_sources(
        &mut sources,
        codex_core::config::capture_managed_instruction_sources(&config).await?,
    )?;
    if args.include_cloud {
        let loader = codex_cloud_config::cloud_config_bundle_loader_for_storage_without_cache(
            config.auth_config(),
            false,
        )
        .await?;
        extend_sources(
            &mut sources,
            codex_core::config::capture_cloud_managed_sources(&loader).await?,
        )?;
    }
    let hook_sources = plugins.effective_plugin_hook_sources();
    extend_sources(
        &mut sources,
        codex_hooks::capture_hook_sources(&config.config_layer_stack, &hook_sources)
            .map_err(anyhow::Error::msg)?,
    )?;
    for source in &hook_sources {
        extend_sources(
            &mut sources,
            codex_skills_extension::capture_package_assets(
                source.plugin_root.as_path(),
                SourceKind::HookAsset,
            )
            .map_err(anyhow::Error::msg)?,
        )?;
    }
    let provider = Arc::new(codex_home::CodexHomeUserInstructionsProvider::new(
        config.codex_home.clone(),
    ));
    let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
    codex_git_attribution::install(
        &mut extensions,
        config.fork.git_attribution.commit_trailer.clone(),
        config.fork.git_attribution.pr_footer.clone(),
    );
    codex_skills_extension::install(&mut extensions, |config: &Config| {
        codex_skills_extension::SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            max_context_tokens: config.skill_max_context_tokens,
            bundled_skills_enabled: config.bundled_skills_enabled(),
            cloud_skill_enabled: false,
            shadow_selection_enabled: false,
        }
    });
    let prompt = codex_core::build_instruction_prompt(
        config,
        Vec::new(),
        None,
        Arc::new(extensions.build()),
        provider,
    )
    .await?;
    extend_sources(
        &mut sources,
        codex_core::instruction_policy::capture_prompt_sources(&prompt)?,
    )?;
    let captured = PolicySnapshot::new(candidate.config, sources.into_values().collect())?;
    let bytes = serde_json::to_vec_pretty(&captured)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.output)?;
    output.write_all(&bytes)?;
    output.sync_all()?;
    println!(
        "Captured {} sources into {} (snapshot {}). No instruction policy was applied.",
        captured.sources.len(),
        args.output.display(),
        captured.snapshot_id
    );
    Ok(())
}

fn extend_sources(
    sources: &mut BTreeMap<(SourceKind, String), PolicySource>,
    incoming: Vec<PolicySource>,
) -> Result<()> {
    for source in incoming {
        let key = (source.kind.clone(), source.logical_id.clone());
        if let Some(previous) = sources.get(&key) {
            if previous != &source {
                bail!(
                    "instruction source changed during capture: {}",
                    source.logical_id
                );
            }
        } else {
            sources.insert(key, source);
        }
    }
    Ok(())
}
