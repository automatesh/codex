use codex_config::ConstraintError;
use codex_config::LoaderOverrides;
use codex_config::RequirementSource;
use codex_core::config::ConfigBuilder;
use codex_features::Feature;
use codex_protocol::config_types::ApprovalsReviewer;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::TempDir;
use tracing_subscriber::prelude::*;

fn isolated_config_builder(home: &TempDir) -> ConfigBuilder {
    ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .fallback_cwd(Some(home.path().to_path_buf()))
        .without_project_context()
        .loader_overrides(LoaderOverrides {
            managed_config_path: Some(home.path().join("managed_config.toml")),
            system_config_path: Some(home.path().join("system_config.toml")),
            system_requirements_path: Some(home.path().join("requirements.toml")),
            ..LoaderOverrides::without_managed_config_for_tests()
        })
}

#[tokio::test]
async fn telemetry_stays_disabled_for_config_and_cli_overrides() -> std::io::Result<()> {
    for use_cli_overrides in [false, true] {
        for analytics in [None, Some(false), Some(true)] {
            for feedback in [None, Some(false), Some(true)] {
                let home = TempDir::new()?;
                let mut config_toml = String::new();
                let mut cli_overrides = Vec::new();
                for (section, enabled) in [("analytics", analytics), ("feedback", feedback)] {
                    if use_cli_overrides {
                        config_toml.push_str(&format!("[{section}]\nenabled = true\n"));
                        if let Some(enabled) = enabled {
                            cli_overrides.push((
                                format!("{section}.enabled"),
                                toml::Value::Boolean(enabled),
                            ));
                        }
                    } else if let Some(enabled) = enabled {
                        config_toml.push_str(&format!("[{section}]\nenabled = {enabled}\n"));
                    }
                }
                std::fs::write(home.path().join("config.toml"), config_toml)?;
                let config = isolated_config_builder(&home)
                    .cli_overrides(cli_overrides)
                    .build()
                    .await?;

                assert_eq!(
                    (config.analytics_enabled, config.feedback_enabled),
                    (Some(false), false),
                    "cli={use_cli_overrides}, analytics={analytics:?}, feedback={feedback:?}",
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn guardian_is_disabled_by_default() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let config = isolated_config_builder(&home).build().await?;

    assert!(!config.features.enabled(Feature::GuardianApproval));
    assert!(!config.features.enabled(Feature::GuardianV2));
    assert_eq!(config.approvals_reviewer, ApprovalsReviewer::User);
    Ok(())
}

#[tokio::test]
async fn explicit_guardian_settings_are_rejected() -> std::io::Result<()> {
    for (config_toml, setting) in [
        (
            "approvals_reviewer = \"auto_review\"\n",
            "approvals_reviewer=auto_review",
        ),
        (
            "[features]\nguardian_approval = true\n",
            "guardian_approval=true",
        ),
        ("[features]\nguardianv2 = true\n", "guardianv2=true"),
    ] {
        let home = TempDir::new()?;
        std::fs::write(home.path().join("config.toml"), config_toml)?;
        let error = isolated_config_builder(&home)
            .build()
            .await
            .expect_err("unavailable Guardian settings must not silently fall back");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("Guardian is not included"));
        assert!(error.to_string().contains(setting), "{error}");
    }
    Ok(())
}

#[tokio::test]
async fn managed_guardian_requirement_is_rejected_with_its_source() -> std::io::Result<()> {
    for feature in ["guardian_approval", "auto_review"] {
        let home = TempDir::new()?;
        let requirements_path = home.path().join("requirements.toml");
        std::fs::write(
            &requirements_path,
            format!("[features]\n{feature} = true\n"),
        )?;
        let error = isolated_config_builder(&home)
            .build()
            .await
            .expect_err("managed requirements cannot enable an unavailable Guardian");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let constraint = error
            .get_ref()
            .and_then(|error| error.downcast_ref::<ConstraintError>())
            .expect("Guardian rejection must retain the structured constraint");
        let ConstraintError::InvalidValue {
            field_name,
            candidate,
            allowed,
            requirement_source,
        } = constraint
        else {
            panic!("unexpected constraint: {constraint}");
        };
        assert_eq!(*field_name, "features");
        assert_eq!(candidate, "guardian_approval=true");
        assert!(allowed.contains("Guardian is not included"));
        let RequirementSource::SystemRequirementsToml { file } = requirement_source else {
            panic!("unexpected requirements source: {requirement_source:?}");
        };
        assert_eq!(file.to_path_buf(), requirements_path);
    }
    Ok(())
}

struct CountEvents(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CountEvents {
    fn on_event(
        &self,
        _event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn feedback_tags_do_not_evaluate_values_or_emit_events() {
    let events = Arc::new(AtomicUsize::new(0));
    let evaluated_values = AtomicUsize::new(0);
    let subscriber = tracing_subscriber::registry().with(CountEvents(Arc::clone(&events)));

    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("verify the event collector is active");
        assert_eq!(events.load(Ordering::Relaxed), 1);

        codex_core::feedback_tags!(
            first = evaluated_values.fetch_add(1, Ordering::Relaxed),
            second = evaluated_values.fetch_add(1, Ordering::Relaxed),
        );
    });

    assert_eq!(evaluated_values.load(Ordering::Relaxed), 0);
    assert_eq!(events.load(Ordering::Relaxed), 1);
}
