use crate::config::OtelSettings;
use crate::metrics::MetricsClient;
use std::error::Error;
use std::io;
use std::time::Duration;
use tracing_subscriber::Layer;
use tracing_subscriber::registry::LookupSpan;

/// Kept for callers shared with upstream; this fork has no OTEL export provider.
pub struct OtelProvider;

impl OtelProvider {
    pub fn shutdown(&self) {}

    pub async fn shutdown_with_timeout(self, _timeout: Duration) -> io::Result<()> {
        Ok(())
    }

    pub fn try_new(_settings: &OtelSettings) -> Result<Option<Self>, Box<dyn Error>> {
        Ok(None)
    }

    pub fn logger_layer<S>(&self) -> Option<impl Layer<S> + Send + Sync>
    where
        S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
    {
        None::<tracing_subscriber::layer::Identity>
    }

    pub fn logger_export_layer<S>(&self) -> Option<impl Layer<S> + Send + Sync>
    where
        S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
    {
        None::<tracing_subscriber::layer::Identity>
    }

    pub fn tracing_layer<S>(&self) -> Option<impl Layer<S> + Send + Sync>
    where
        S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
    {
        None::<tracing_subscriber::layer::Identity>
    }

    pub fn reloadable_tracing_layer<S>(_service_name: &'static str) -> impl Layer<S> + Send + Sync
    where
        S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
    {
        tracing_subscriber::layer::Identity::new()
    }

    pub fn codex_export_filter(_meta: &tracing::Metadata<'_>) -> bool {
        false
    }
    pub fn log_export_filter(_meta: &tracing::Metadata<'_>) -> bool {
        false
    }
    pub fn trace_export_filter(_meta: &tracing::Metadata<'_>) -> bool {
        false
    }
    pub fn metrics(&self) -> Option<&MetricsClient> {
        None
    }
}
