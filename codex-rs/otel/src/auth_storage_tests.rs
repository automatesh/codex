use super::*;
use crate::metrics_data::InMemoryMetricExporter;
use crate::{MetricsClient, MetricsConfig};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tracing_subscriber::layer::SubscriberExt;

#[test]
fn credential_activity_never_collects_into_an_explicit_local_sink() -> crate::Result<()> {
    const CHILD: &str = "CODEX_AUTH_STORAGE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let thread = std::thread::current();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", thread.name().unwrap(), "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(());
    }
    struct EventCount(Arc<AtomicUsize>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventCount {
        fn on_event(&self, _: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let metrics = crate::install_global_metrics(MetricsClient::new(
        MetricsConfig::in_memory("test", "test", "1", InMemoryMetricExporter::default())
            .with_runtime_reader(),
    )?);
    metrics.counter("test.control", 1, &[])?;
    assert!(metrics.snapshot()?.scope_metrics().any(|scope| {
        scope
            .metrics()
            .any(|metric| metric.name() == "test.control")
    }));
    let events = Arc::new(AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(EventCount(Arc::clone(&events)));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "test_collector", "collector is active");
        for operation in [
            Operation::Load,
            Operation::Save,
            Operation::Delete,
            Operation::Cleanup,
            Operation::RefreshPersist,
        ] {
            let mut telemetry = StorageTelemetry::new(
                CredentialKind::Codex,
                StoreMode::Auto,
                Store::Secrets,
                operation,
                AuthStorageOriginator::from_client_name("private-client"),
            )
            .with_phase(StoragePhase::Pinned);
            telemetry.record_secure_error(&std::io::Error::other("private-error"));
            telemetry
                .record_load_attempt(Store::Secrets, &Err::<Option<String>, _>("private-error"));
            telemetry.record_load_attempt(Store::File, &Ok::<_, &str>(Some("private-credential")));
            telemetry.record_save_attempt(Store::Secrets, &Err::<(), _>("private-error"));
            telemetry.record_save_attempt(Store::File, &Ok::<(), &str>(()));
            telemetry.record_delete_attempt(Store::Secrets, &Ok::<_, &str>(true));
        }
        let mut fallback = StorageTelemetry::new(
            CredentialKind::Mcp,
            StoreMode::Auto,
            Store::Secrets,
            Operation::Save,
            AuthStorageOriginator::from_client_name("private-client"),
        );
        fallback.record_save_attempt(Store::Secrets, &Err::<(), _>("private-error"));
        fallback.record_save_attempt(Store::File, &Ok::<(), &str>(()));
    });
    assert_eq!(events.load(Ordering::Relaxed), 1);
    assert_eq!(std::mem::size_of::<StorageTelemetry>(), 0);
    assert!(
        metrics
            .snapshot()?
            .scope_metrics()
            .all(|scope| scope.metrics().next().is_none())
    );
    metrics.shutdown()
}
