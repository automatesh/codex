use crate::config_manager::ConfigManager;
use codex_login::AuthManager;
use codex_otel::OtelProvider;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::reload;

type OtelExportLayer<S> = Option<Box<dyn Layer<S> + Send + Sync + 'static>>;
type OtelReloadLayers<S> = (
    Vec<Box<dyn Layer<S> + Send + Sync + 'static>>,
    reload::Handle<OtelExportLayer<S>, S>,
);

pub(crate) fn layers<S>(_provider: Option<&OtelProvider>) -> OtelReloadLayers<S>
where
    S: Subscriber + for<'span> LookupSpan<'span> + Send + Sync + 'static,
{
    let (_layer, handle) = reload::Layer::new(None);
    (Vec::new(), handle)
}

pub(crate) fn spawn<S>(
    _provider: Option<OtelProvider>,
    _logger_reload_handle: reload::Handle<OtelExportLayer<S>, S>,
    _config_manager: ConfigManager,
    _auth_manager: Arc<AuthManager>,
    _default_analytics_enabled: bool,
    _shutdown_token: CancellationToken,
) -> JoinHandle<()>
where
    S: Subscriber + for<'span> LookupSpan<'span> + Send + Sync + 'static,
{
    tokio::spawn(async {})
}
