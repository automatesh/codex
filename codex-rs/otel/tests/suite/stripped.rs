use codex_http_client::{HttpClientFactory, OutboundProxyPolicy};
use codex_otel::{
    MetricsClient, MetricsConfig, MetricsError, OtelExporter, OtelHttpProtocol, OtelProvider,
    OtelSettings,
};
use std::io::ErrorKind;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use codex_otel::SessionTelemetry;
use codex_otel::SkillInvocationEvent;
use codex_otel::SkillInvocationType;
use codex_protocol::ThreadId;
use codex_protocol::ToolName;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SkillScope;
use tracing_subscriber::layer::SubscriberExt;

#[test]
fn local_snapshots_preserve_integer_precision_and_reset_counter_deltas() {
    use codex_otel::metrics_data::AggregatedMetrics;
    use codex_otel::metrics_data::InMemoryMetricExporter;
    use codex_otel::metrics_data::MetricData;

    let metrics = MetricsClient::new(
        MetricsConfig::in_memory("test", "test", "1", InMemoryMetricExporter::default())
            .with_runtime_reader(),
    )
    .unwrap();
    metrics.counter("test.count", i64::MAX, &[]).unwrap();
    metrics.gauge("test.gauge", i64::MIN, &[]).unwrap();
    let snapshot = metrics.snapshot().unwrap();
    let counter = crate::harness::find_metric(&snapshot, "test.count").unwrap();
    let AggregatedMetrics::U64(MetricData::Sum(sum)) = counter.data() else {
        panic!("expected counter");
    };
    assert_eq!(sum.data_points().next().unwrap().value(), i64::MAX as u64);
    let gauge = crate::harness::find_metric(&snapshot, "test.gauge").unwrap();
    let AggregatedMetrics::I64(MetricData::Gauge(gauge)) = gauge.data() else {
        panic!("expected gauge");
    };
    assert_eq!(gauge.data_points().next().unwrap().value(), i64::MIN);
    assert!(
        metrics
            .snapshot()
            .unwrap()
            .scope_metrics()
            .all(|scope| scope.metrics().next().is_none())
    );
}

#[test]
fn session_activity_never_emits_telemetry_into_an_active_subscriber() {
    struct EventTargets(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventTargets {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.0
                .lock()
                .unwrap()
                .push(event.metadata().target().to_string());
        }
    }

    let targets = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(EventTargets(Arc::clone(&targets)));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "stripped_test", "subscriber is active");
        let telemetry = SessionTelemetry::new(
            ThreadId::new(),
            "test-model",
            "test-model",
            Some("test-account".to_string()),
            Some("test@example.invalid".to_string()),
            None,
            "test_originator".to_string(),
            /*log_user_prompts*/ true,
            "tty".to_string(),
            SessionSource::Cli,
        );
        telemetry.tool_result_with_tags(
            &ToolName::plain("exec_command"),
            "test-call",
            r#"{"cmd":"echo test"}"#,
            Duration::from_millis(7),
            true,
            "test output",
            &[("sandbox", "workspace")],
            &[("mcp_server", "test-server")],
        );
        for invocation_type in [SkillInvocationType::Explicit, SkillInvocationType::Implicit] {
            telemetry.skill_invocation(SkillInvocationEvent {
                turn_id: "test-turn",
                skill_name: "test-skill",
                scope: Some(SkillScope::Repo),
                plugin_id: Some("test-plugin"),
                invocation_type,
            });
        }
        telemetry.sandbox_outcome(
            "exec_command",
            "test-call",
            "success",
            Duration::from_millis(7),
            None,
        );
    });
    assert_eq!(*targets.lock().unwrap(), vec!["stripped_test".to_string()]);
}

#[test]
fn exporter_configuration_cannot_enable_network_telemetry() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let exporters = [
        OtelExporter::None,
        OtelExporter::Statsig,
        OtelExporter::OtlpGrpc {
            endpoint: endpoint.clone(),
            headers: Default::default(),
            tls: None,
        },
        OtelExporter::OtlpHttp {
            endpoint: endpoint.clone(),
            headers: Default::default(),
            protocol: OtelHttpProtocol::Binary,
            tls: None,
        },
        OtelExporter::OtlpHttp {
            endpoint,
            headers: Default::default(),
            protocol: OtelHttpProtocol::Json,
            tls: None,
        },
    ];
    for exporter in exporters {
        let settings = OtelSettings {
            http_client_factory: HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            environment: "test".into(),
            service_name: "test".into(),
            service_version: "1".into(),
            codex_home: ".".into(),
            exporter: exporter.clone(),
            trace_exporter: exporter.clone(),
            metrics_exporter: exporter.clone(),
            runtime_metrics: true,
            span_attributes: Default::default(),
            tracestate: Default::default(),
        };
        assert!(OtelProvider::try_new(&settings).unwrap().is_none());
        assert!(matches!(
            MetricsClient::new(MetricsConfig::otlp("test", "test", "1", exporter)),
            Err(MetricsError::ExporterDisabled)
        ));
    }
    assert!(codex_otel::global_statsig_metrics_settings().is_none());
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
}
