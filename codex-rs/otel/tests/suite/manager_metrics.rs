use codex_otel::metrics_data::InMemoryMetricExporter;
use codex_otel::{MetricsClient, MetricsConfig, Result, SessionTelemetry, TelemetryAuthMode};
use codex_protocol::protocol::SessionSource;
use codex_protocol::{ThreadId, ToolName};
use std::time::Duration;

#[test]
fn activity_does_not_collect_even_with_an_explicit_local_sink() -> Result<()> {
    let metrics = MetricsClient::new(
        MetricsConfig::in_memory("test", "test", "1", InMemoryMetricExporter::default())
            .with_runtime_reader(),
    )?;
    metrics.counter("test.control", 1, &[])?;
    assert!(crate::harness::find_metric(&metrics.snapshot()?, "test.control").is_some());
    let manager = SessionTelemetry::new(
        ThreadId::new(),
        "private-model",
        "private-slug",
        Some("private-account".into()),
        Some("private-email".into()),
        Some(TelemetryAuthMode::ApiKey),
        "private-originator".into(),
        true,
        "private-terminal".into(),
        SessionSource::Cli,
    )
    .with_metrics(metrics.clone())
    .with_product_sku(Some("codex"))
    .with_metrics_service_name("private-service");
    manager.counter("test.attached", 1, &[]);
    assert!(crate::harness::find_metric(&metrics.snapshot()?, "test.attached").is_some());
    let events = crate::harness::capture_events(|| {
        manager.tool_result_with_tags(
            &ToolName::plain("shell"),
            "private-call",
            r#"{"cmd":"private-command"}"#,
            Duration::from_millis(7),
            true,
            "private-output",
            &[],
            &[],
        );
        manager.record_plugin_install_suggestion(
            "plugin",
            "private-plugin",
            "private-name",
            "accept",
            true,
            true,
        );
        manager.record_plugin_install_elicitation_sent("plugin", "private-plugin", "private-name");
        manager.record_multi_agent_spawn_phase(
            "startup",
            Duration::from_millis(3),
            "all",
            "full",
            "v2",
        );
        manager.record_multi_agent_spawn_failure(
            "internal",
            &std::io::Error::other("private-error").into(),
            "private-call",
            "private-turn",
            "all",
            "v2",
        );
        manager.record_startup_phase("startup", Duration::from_millis(3), Some("ok"));
        manager.record_turn_ttft(Duration::from_millis(3));
        manager.record_turn_cost("private-turn", "0.001", false, Some("fast"), Some("high"));
    });
    assert!(events.is_empty(), "unexpected activity events: {events:?}");
    assert!(
        metrics
            .snapshot()?
            .scope_metrics()
            .all(|scope| scope.metrics().next().is_none())
    );
    Ok(())
}
