use crate::TelemetryAuthMode;
use crate::ToolDecisionSource;
use crate::metrics::MetricsClient;
use crate::metrics::MetricsConfig;
use crate::metrics::MetricsError;
use crate::metrics::Result as MetricsResult;
use crate::metrics::runtime_metrics::RuntimeMetricsSummary;
use crate::metrics::timer::Timer;
use crate::metrics_data::ResourceMetrics;
use crate::provider::OtelProvider;
use codex_api::AgentIdentityTelemetry;
use codex_api::ApiError;
use codex_api::ResponseEvent;
use codex_protocol::ThreadId;
use codex_protocol::ToolName;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::ToolResultLogConfig;
use codex_protocol::error::CodexErr;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::user_input::UserInput;
use eventsource_stream::Event as StreamEvent;
use eventsource_stream::EventStreamError as StreamError;
use reqwest::Error;
use reqwest::Response;
use std::future::Future;
use std::time::Duration;
use tokio::time::error::Elapsed;
use tracing::Span;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthEnvTelemetryMetadata {
    pub openai_api_key_env_present: bool,
    pub codex_api_key_env_present: bool,
    pub codex_api_key_env_enabled: bool,
    pub provider_env_key_name: Option<String>,
    pub provider_env_key_present: Option<bool>,
    pub refresh_token_url_override_present: bool,
}

#[derive(Debug, Clone)]
pub struct SessionTelemetryMetadata {
    pub(crate) conversation_id: ThreadId,
    // Full canonical path when available; legacy nickname or thread ID otherwise.
    // Keep this potentially user-chosen label on detailed logs, not trace-safe events.
    pub(crate) agent_name: String,
    pub(crate) auth_mode: Option<String>,
    pub(crate) auth_env: AuthEnvTelemetryMetadata,
    pub(crate) account_id: Option<String>,
    pub(crate) user_id: Option<String>,
    pub(crate) account_email: Option<String>,
    pub(crate) originator: String,
    pub(crate) product_sku: Option<&'static str>,
    pub(crate) service_name: Option<String>,
    pub(crate) session_source: String,
    pub(crate) model: String,
    pub(crate) slug: String,
    pub(crate) service_tier: Option<String>,
    pub(crate) model_reasoning_effort: Option<String>,
    pub(crate) log_user_prompts: bool,
    pub(crate) app_version: &'static str,
    pub(crate) terminal_type: String,
}

#[derive(Debug, Clone)]
pub struct SessionTelemetry {
    tool_result_log_config: ToolResultLogConfig,
    pub(crate) metadata: SessionTelemetryMetadata,
    pub(crate) metrics: Option<MetricsClient>,
    pub(crate) metrics_use_metadata_tags: bool,
}

/// Bounds product SKU attribution before it is used in telemetry dimensions.
pub fn bounded_product_sku(product_sku: Option<&str>) -> Option<&'static str> {
    const KNOWN_PRODUCT_SKUS: &[&str] = &["codex"];

    match product_sku {
        None | Some("") => None,
        Some(sku) => Some(
            KNOWN_PRODUCT_SKUS
                .iter()
                .copied()
                .find(|known| *known == sku)
                .unwrap_or("other"),
        ),
    }
}

impl SessionTelemetry {
    pub fn with_tool_result_log_config(self, _config: ToolResultLogConfig) -> Self {
        self
    }

    pub fn with_auth_env(self, _auth_env: AuthEnvTelemetryMetadata) -> Self {
        self
    }

    pub fn with_model(self, _model: &str, _slug: &str) -> Self {
        self
    }

    pub fn with_inference_request(
        self,
        _service_tier: Option<&str>,
        _model_reasoning_effort: Option<&ReasoningEffort>,
    ) -> Self {
        self
    }

    pub fn with_metrics_service_name(self, _service_name: &str) -> Self {
        self
    }

    /// Attributes bounded telemetry without turning arbitrary configuration into metric labels.
    pub fn with_product_sku(self, _product_sku: Option<&str>) -> Self {
        self
    }

    pub fn with_metrics(mut self, metrics: MetricsClient) -> Self {
        self.metrics = Some(metrics);
        self.metrics_use_metadata_tags = true;
        self
    }

    pub fn with_metrics_without_metadata_tags(mut self, metrics: MetricsClient) -> Self {
        self.metrics = Some(metrics);
        self.metrics_use_metadata_tags = false;
        self
    }

    pub fn with_metrics_config(self, config: MetricsConfig) -> MetricsResult<Self> {
        let metrics = MetricsClient::new(config)?;
        Ok(self.with_metrics(metrics))
    }

    pub fn with_provider_metrics(self, provider: &OtelProvider) -> Self {
        match provider.metrics() {
            Some(metrics) => self.with_metrics(metrics.clone()),
            None => self,
        }
    }

    pub fn counter(&self, name: &str, inc: i64, tags: &[(&str, &str)]) {
        let res: MetricsResult<()> = (|| {
            let Some(metrics) = &self.metrics else {
                return Ok(());
            };

            let tags = self.tags_with_metadata(tags)?;
            metrics.counter(name, inc, &tags)
        })();

        if let Err(e) = res {
            tracing::warn!("metrics counter [{name}] failed: {e}");
        }
    }

    pub fn histogram(&self, name: &str, value: i64, tags: &[(&str, &str)]) {
        let res: MetricsResult<()> = (|| {
            let Some(metrics) = &self.metrics else {
                return Ok(());
            };

            let tags = self.tags_with_metadata(tags)?;
            metrics.histogram(name, value, &tags)
        })();

        if let Err(e) = res {
            tracing::warn!("metrics histogram [{name}] failed: {e}");
        }
    }

    /// Records a histogram with explicit buckets and the usual session attribution.
    /// All callers of the same metric name must use the same boundaries.
    pub fn histogram_with_boundaries(
        &self,
        name: &str,
        value: i64,
        boundaries: &[f64],
        tags: &[(&str, &str)],
    ) {
        let res: MetricsResult<()> = (|| {
            let Some(metrics) = &self.metrics else {
                return Ok(());
            };

            let tags = self.tags_with_metadata(tags)?;
            metrics.histogram_with_boundaries(name, value, boundaries, &tags)
        })();

        if let Err(e) = res {
            tracing::warn!("metrics histogram [{name}] failed: {e}");
        }
    }

    pub fn record_duration(&self, name: &str, duration: Duration, tags: &[(&str, &str)]) {
        let res: MetricsResult<()> = (|| {
            let Some(metrics) = &self.metrics else {
                return Ok(());
            };

            let tags = self.tags_with_metadata(tags)?;
            metrics.record_duration(name, duration, &tags)
        })();

        if let Err(e) = res {
            tracing::warn!("metrics duration [{name}] failed: {e}");
        }
    }

    /// Records one successful multi-agent spawn phase with bounded spawn dimensions.
    pub fn record_multi_agent_spawn_phase(
        &self,
        _phase: &'static str,
        _duration: Duration,
        _fork_mode: &'static str,
        _history_mode: &'static str,
        _multi_agent_version: &'static str,
    ) {
    }

    /// Records one failed multi-agent spawn after request validation has completed.
    pub fn record_multi_agent_spawn_failure(
        &self,
        _reason: &'static str,
        _err: &CodexErr,
        _call_id: &str,
        _turn_id: &str,
        _fork_mode: &'static str,
        _multi_agent_version: &'static str,
    ) {
    }

    /// Records a coarse startup phase for production latency breakdowns.
    pub fn record_startup_phase(
        &self,
        _phase: &'static str,
        _duration: Duration,
        _status: Option<&'static str>,
    ) {
    }

    /// Records time to first token as both a metric and a production telemetry event.
    pub fn record_turn_ttft(&self, _duration: Duration) {}

    pub fn record_turn_cost(
        &self,
        _turn_id: &str,
        _estimated_usd: &str,
        _interrupted: bool,
        _speed: Option<&str>,
        _reasoning_effort: Option<&str>,
    ) {
    }

    /// Records the moment a plugin or connector install elicitation is dispatched.
    pub fn record_plugin_install_elicitation_sent(
        &self,
        _tool_type: &str,
        _tool_id: &str,
        _tool_name: &str,
    ) {
    }

    /// Records the outcome of a surfaced plugin or connector install suggestion.
    pub fn record_plugin_install_suggestion(
        &self,
        _tool_type: &str,
        _tool_id: &str,
        _tool_name: &str,
        _response_action: &str,
        _user_confirmed: bool,
        _completed: bool,
    ) {
    }

    pub fn start_timer(&self, name: &str, tags: &[(&str, &str)]) -> Result<Timer, MetricsError> {
        let Some(metrics) = &self.metrics else {
            return Err(MetricsError::ExporterDisabled);
        };
        let tags = self.tags_with_metadata(tags)?;
        metrics.start_timer(name, &tags)
    }

    pub fn shutdown_metrics(&self) -> MetricsResult<()> {
        let Some(metrics) = &self.metrics else {
            return Ok(());
        };
        metrics.shutdown()
    }

    pub fn snapshot_metrics(&self) -> MetricsResult<ResourceMetrics> {
        let Some(metrics) = &self.metrics else {
            return Err(MetricsError::ExporterDisabled);
        };
        metrics.snapshot()
    }

    /// Collect and discard a runtime metrics snapshot to reset delta accumulators.
    pub fn reset_runtime_metrics(&self) {
        if self.metrics.is_none() {
            return;
        }
        if let Err(err) = self.snapshot_metrics() {
            tracing::debug!("runtime metrics reset skipped: {err}");
        }
    }

    /// Collect a runtime metrics summary if debug snapshots are available.
    pub fn runtime_metrics_summary(&self) -> Option<RuntimeMetricsSummary> {
        let snapshot = match self.snapshot_metrics() {
            Ok(snapshot) => snapshot,
            Err(_) => {
                return None;
            }
        };
        let summary = RuntimeMetricsSummary::from_snapshot(&snapshot);
        if summary.is_empty() {
            None
        } else {
            Some(summary)
        }
    }

    fn tags_with_metadata<'a>(
        &'a self,
        tags: &'a [(&'a str, &'a str)],
    ) -> MetricsResult<Vec<(&'a str, &'a str)>> {
        let mut merged = tags.to_vec();
        merged.extend(self.metadata_tag_refs()?);
        Ok(merged)
    }

    fn metadata_tag_refs(&self) -> MetricsResult<Vec<(&str, &str)>> {
        Ok(Vec::new())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        _conversation_id: ThreadId,
        _model: &str,
        _slug: &str,
        _account_id: Option<String>,
        _account_email: Option<String>,
        _auth_mode: Option<TelemetryAuthMode>,
        _originator: String,
        _log_user_prompts: bool,
        _terminal_type: String,
        _session_source: SessionSource,
    ) -> SessionTelemetry {
        Self {
            tool_result_log_config: ToolResultLogConfig::default(),
            metadata: SessionTelemetryMetadata {
                conversation_id: ThreadId::from_u128(0),
                agent_name: String::new(),
                auth_mode: None,
                auth_env: AuthEnvTelemetryMetadata::default(),
                account_id: None,
                user_id: None,
                account_email: None,
                originator: String::new(),
                product_sku: None,
                service_name: None,
                session_source: String::new(),
                model: String::new(),
                slug: String::new(),
                service_tier: None,
                model_reasoning_effort: None,
                log_user_prompts: false,
                app_version: "",
                terminal_type: String::new(),
            },
            metrics: None,
            metrics_use_metadata_tags: true,
        }
    }

    pub fn record_responses(&self, _handle_responses_span: &Span, _event: &ResponseEvent) {}

    #[allow(clippy::too_many_arguments)]
    pub fn conversation_starts(
        &self,
        _provider_name: &str,
        _reasoning_effort: Option<ReasoningEffort>,
        _reasoning_summary: ReasoningSummary,
        _context_window: Option<i64>,
        _auto_compact_token_limit: Option<i64>,
        _approval_policy: AskForApproval,
        _sandbox_policy: SandboxPolicy,
        _mcp_servers: Vec<&str>,
    ) {
    }

    pub async fn log_request<F, Fut>(&self, _attempt: u64, f: F) -> Result<Response, Error>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Response, Error>>,
    {
        f().await
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_api_request(
        &self,
        _attempt: u64,
        _status: Option<u16>,
        _error: Option<&str>,
        _duration: Duration,
        _auth_header_attached: bool,
        _auth_header_name: Option<&str>,
        _retry_after_unauthorized: bool,
        _recovery_mode: Option<&str>,
        _recovery_phase: Option<&str>,
        _endpoint: &str,
        _request_id: Option<&str>,
        _cf_ray: Option<&str>,
        _auth_error: Option<&str>,
        _auth_error_code: Option<&str>,
        _agent_identity_telemetry: Option<&AgentIdentityTelemetry>,
    ) {
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_websocket_connect(
        &self,
        _duration: Duration,
        _status: Option<u16>,
        _error: Option<&str>,
        _auth_header_attached: bool,
        _auth_header_name: Option<&str>,
        _retry_after_unauthorized: bool,
        _recovery_mode: Option<&str>,
        _recovery_phase: Option<&str>,
        _endpoint: &str,
        _connection_reused: bool,
        _request_id: Option<&str>,
        _cf_ray: Option<&str>,
        _auth_error: Option<&str>,
        _auth_error_code: Option<&str>,
        _agent_identity_telemetry: Option<&AgentIdentityTelemetry>,
    ) {
    }

    pub fn record_websocket_request(
        &self,
        _duration: Duration,
        _error: Option<&str>,
        _connection_reused: bool,
        _agent_identity_telemetry: Option<&AgentIdentityTelemetry>,
    ) {
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_auth_recovery(
        &self,
        _mode: &str,
        _step: &str,
        _outcome: &str,
        _request_id: Option<&str>,
        _cf_ray: Option<&str>,
        _auth_error: Option<&str>,
        _auth_error_code: Option<&str>,
        _recovery_reason: Option<&str>,
        _auth_state_changed: Option<bool>,
    ) {
    }

    pub fn record_websocket_event(
        &self,
        _result: &Result<
            Option<
                Result<
                    tokio_tungstenite::tungstenite::Message,
                    tokio_tungstenite::tungstenite::Error,
                >,
            >,
            ApiError,
        >,
        _duration: Duration,
    ) {
    }

    pub fn log_sse_event<E>(
        &self,
        _response: &Result<Option<Result<StreamEvent, StreamError<E>>>, Elapsed>,
        _duration: Duration,
    ) where
        E: std::fmt::Display,
    {
    }

    pub fn sse_event_failed<T>(&self, _kind: Option<&String>, _duration: Duration, _error: &T)
    where
        T: std::fmt::Display,
    {
    }

    pub fn see_event_completed_failed<T>(&self, _error: &T)
    where
        T: std::fmt::Display,
    {
    }

    pub fn sse_event_completed(&self, _usage: &TokenUsage, _ttft_ms: Option<i64>) {}

    pub fn user_prompt(&self, _items: &[UserInput]) {}

    pub fn tool_decision(
        &self,
        _tool_name: &ToolName,
        _call_id: &str,
        _decision: &ReviewDecision,
        _source: Option<ToolDecisionSource>,
    ) {
    }

    pub fn sandbox_outcome(
        &self,
        _tool_name: &str,
        _call_id: &str,
        _outcome: &str,
        _initial_duration: Duration,
        _escalated_duration: Option<Duration>,
    ) {
    }

    /// Runs the operation without evaluating the diagnostic-output callback.
    #[allow(clippy::too_many_arguments)]
    pub async fn log_tool_result_with_tags<T, F, Fut, E>(
        &self,
        _tool_name: &ToolName,
        _call_id: &str,
        _arguments: &str,
        _extra_tags: &[(&str, &str)],
        _extra_trace_fields: &[(&str, &str)],
        f: F,
        _log_output: impl FnOnce(&T) -> (String, bool),
    ) -> Result<T, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        f().await
    }

    #[allow(clippy::too_many_arguments)]
    pub fn tool_result_with_tags(
        &self,
        _tool_name: &ToolName,
        _call_id: &str,
        _arguments: &str,
        _duration: Duration,
        _success: bool,
        _output: &str,
        _extra_tags: &[(&str, &str)],
        _extra_trace_fields: &[(&str, &str)],
    ) {
    }
}
