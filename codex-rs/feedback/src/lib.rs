use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fs;
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use codex_http_client::HttpClientFactory;
use codex_login::AuthEnvTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_state::LogWriteFailureReporter;
use tracing::Level;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::registry::LookupSpan;

mod daemon_logs;
pub use daemon_logs::daemon_log_attachments;
pub(crate) mod feedback_diagnostics;
mod guardian;
mod report_upload;
pub use feedback_diagnostics::FEEDBACK_DIAGNOSTICS_ATTACHMENT_FILENAME;
pub use feedback_diagnostics::FeedbackDiagnostic;
pub use feedback_diagnostics::FeedbackDiagnostics;
pub use guardian::GuardianReviewFailures;
pub use guardian::GuardianReviewRecord;
pub use guardian::guardian_review_failures;
pub use guardian::record_guardian_review_failure;
pub use report_upload::FeedbackDelivery;
pub use report_upload::FeedbackTransport;
pub use report_upload::prepare_report_attachment;

/// Filename used for the redacted `codex doctor --json` feedback attachment.
pub const DOCTOR_REPORT_ATTACHMENT_FILENAME: &str = "codex-doctor-report.json";
/// Filename used for the raw Codex Apps MCP tools cache feedback attachment.
pub const CODEX_APPS_TOOLS_CACHE_ATTACHMENT_FILENAME: &str = "codex-apps-tools-cache.json";
/// Filename used for the raw connector directory cache feedback attachment.
pub const CODEX_APP_DIRECTORY_CACHE_ATTACHMENT_FILENAME: &str = "codex-app-directory-cache.json";
/// Filename used for the Windows sandbox log feedback attachment.
pub const WINDOWS_SANDBOX_LOG_ATTACHMENT_FILENAME: &str = "windows-sandbox.log";
const DEFAULT_MAX_BYTES: usize = 4 * 1024 * 1024; // 4 MiB
// Raw collection budgets used by the report API, not the interactive upload.
pub const MAX_ATTACHMENT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_ATTACHMENTS_BYTES: usize = 126 * 1024 * 1024;

/// Structured request/auth fields that should be attached to feedback uploads.
pub struct FeedbackRequestTags<'a> {
    pub endpoint: &'a str,
    pub auth_header_attached: bool,
    pub auth_header_name: Option<&'a str>,
    pub auth_mode: Option<&'a str>,
    pub auth_retry_after_unauthorized: Option<bool>,
    pub auth_recovery_mode: Option<&'a str>,
    pub auth_recovery_phase: Option<&'a str>,
    pub auth_connection_reused: Option<bool>,
    pub auth_request_id: Option<&'a str>,
    pub auth_cf_ray: Option<&'a str>,
    pub auth_error: Option<&'a str>,
    pub auth_error_code: Option<&'a str>,
    pub auth_recovery_followup_success: Option<bool>,
    pub auth_recovery_followup_status: Option<u16>,
}

pub fn emit_feedback_request_tags(_tags: &FeedbackRequestTags<'_>) {}

pub fn emit_feedback_request_tags_with_auth_env(
    _tags: &FeedbackRequestTags<'_>,
    _auth_env: &AuthEnvTelemetry,
) {
}

#[derive(Clone)]
pub struct CodexFeedback {
    inner: Arc<FeedbackInner>,
}

impl LogWriteFailureReporter for CodexFeedback {
    fn report_failure(&self, diagnostic: &str) {
        // Bypass tracing so this diagnostic cannot return to the SQLite writer.
        self.inner
            .ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_bytes(diagnostic.as_bytes());
    }
}

impl Default for CodexFeedback {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexFeedback {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_BYTES)
    }

    pub(crate) fn with_capacity(max_bytes: usize) -> Self {
        Self {
            inner: Arc::new(FeedbackInner::new(max_bytes)),
        }
    }

    pub fn make_writer(&self) -> FeedbackMakeWriter {
        FeedbackMakeWriter {
            inner: self.inner.clone(),
        }
    }

    /// Returns a [`tracing_subscriber`] layer that captures diagnostic logs into this feedback
    /// ring buffer.
    ///
    /// This is intended for initialization code so call sites don't have to duplicate the exact
    /// `fmt::layer()` configuration and filter logic.
    pub fn logger_layer<S>(&self) -> impl Layer<S> + Send + Sync + 'static
    where
        S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    {
        tracing_subscriber::fmt::layer()
            .with_writer(self.make_writer())
            .with_timer(tracing_subscriber::fmt::time::SystemTime)
            .with_ansi(false)
            .with_target(false)
            // Capture diagnostics independently of `RUST_LOG` without filling the feedback ring
            // with high-volume request and response payloads.
            .with_filter(
                Targets::new()
                    .with_default(Level::TRACE)
                    // Opted-in content belongs to the configured OTLP destination, not feedback.
                    .with_target("codex_otel.log_only", LevelFilter::OFF)
                    .with_target("codex_http_client::transport", LevelFilter::DEBUG)
                    .with_target("codex_api::sse", LevelFilter::DEBUG)
                    // `tracing-log` checks legacy log records against their original
                    // target before re-emitting them as `log`; tungstenite TRACE
                    // includes full websocket frames and authenticated handshakes.
                    .with_target("tungstenite", LevelFilter::DEBUG)
                    .with_target("codex_api::responses_websocket_timing", LevelFilter::OFF)
                    .with_target("codex_core::post_sampling_token_estimate", LevelFilter::OFF),
            )
    }

    pub fn metadata_layer<S>(&self) -> impl Layer<S> + Send + Sync + 'static
    where
        S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    {
        tracing_subscriber::layer::Identity::new()
    }

    pub fn snapshot(&self, session_id: Option<ThreadId>) -> FeedbackSnapshot {
        let bytes = {
            #[allow(clippy::expect_used)]
            let guard = self.inner.ring.lock().expect("mutex poisoned");
            guard.snapshot_bytes()
        };
        FeedbackSnapshot {
            bytes,
            feedback_diagnostics: FeedbackDiagnostics::collect_from_env(),
            thread_id: session_id
                .map(|id| id.to_string())
                .unwrap_or("no-active-thread-".to_string() + &ThreadId::new().to_string()),
        }
    }
}

struct FeedbackInner {
    ring: Mutex<RingBuffer>,
}

impl FeedbackInner {
    fn new(max_bytes: usize) -> Self {
        Self {
            ring: Mutex::new(RingBuffer::new(max_bytes)),
        }
    }
}

#[derive(Clone)]
pub struct FeedbackMakeWriter {
    inner: Arc<FeedbackInner>,
}

impl<'a> MakeWriter<'a> for FeedbackMakeWriter {
    type Writer = FeedbackWriter;

    fn make_writer(&'a self) -> Self::Writer {
        FeedbackWriter {
            inner: self.inner.clone(),
        }
    }
}

pub struct FeedbackWriter {
    inner: Arc<FeedbackInner>,
}

impl Write for FeedbackWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self.inner.ring.lock().map_err(|_| io::ErrorKind::Other)?;
        guard.push_bytes(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct RingBuffer {
    max: usize,
    buf: VecDeque<u8>,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            max: capacity,
            buf: VecDeque::with_capacity(capacity),
        }
    }

    fn len(&self) -> usize {
        self.buf.len()
    }

    fn push_bytes(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        // If the incoming chunk is larger than capacity, keep only the trailing bytes.
        if data.len() >= self.max {
            self.buf.clear();
            let start = data.len() - self.max;
            self.buf.extend(data[start..].iter().copied());
            return;
        }

        // Evict from the front if we would exceed capacity.
        let needed = self.len() + data.len();
        if needed > self.max {
            let to_drop = needed - self.max;
            for _ in 0..to_drop {
                let _ = self.buf.pop_front();
            }
        }

        self.buf.extend(data.iter().copied());
    }

    fn snapshot_bytes(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }
}

pub struct FeedbackSnapshot {
    bytes: Vec<u8>,
    feedback_diagnostics: FeedbackDiagnostics,
    pub thread_id: String,
}

pub struct FeedbackAttachmentPath {
    pub path: PathBuf,
    /// Optional filename to use for the uploaded attachment instead of `path`'s basename.
    pub attachment_filename_override: Option<String>,
}

#[cfg(test)]
#[path = "rollout_attachment_tests.rs"]
mod rollout_attachment_tests;

impl FeedbackAttachmentPath {
    /// Read a whole regular file within the caller's size limit.
    pub fn read_attachment(&self, max_bytes: usize) -> io::Result<Option<FeedbackAttachment>> {
        let rollout_path = codex_rollout::rollout_id_from_path(&self.path)
            .map(|_| codex_rollout::plain_rollout_path(&self.path));
        let buffer = if rollout_path.is_some() {
            let Some(buffer) =
                codex_rollout::read_rollout_prefix(&self.path, max_bytes.saturating_add(1))?
            else {
                tracing::error!(
                    "feedback attachment skipped: rollout is missing or not a regular file"
                );
                return Ok(None);
            };
            buffer
        } else {
            let metadata = fs::metadata(&self.path)?;
            if !metadata.is_file() {
                tracing::error!("feedback attachment skipped: not a regular file");
                return Ok(None);
            }
            if metadata.len() > max_bytes as u64 {
                tracing::error!(
                    bytes = metadata.len(),
                    max_bytes,
                    "feedback attachment skipped: size limit exceeded"
                );
                return Ok(None);
            }
            let mut buffer = Vec::new();
            // Detect files that grow past the limit after the metadata check.
            fs::File::open(&self.path)?
                .take(max_bytes as u64 + 1)
                .read_to_end(&mut buffer)?;
            buffer
        };
        if buffer.len() > max_bytes {
            tracing::error!(
                bytes_read = buffer.len(),
                max_bytes,
                "feedback attachment skipped: decoded size limit exceeded"
            );
            return Ok(None);
        }
        let filename = self
            .attachment_filename_override
            .clone()
            .unwrap_or_else(|| {
                rollout_path
                    .as_ref()
                    .unwrap_or(&self.path)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| "extra-log.log".to_string())
            });
        let content_type = match Path::new(&filename)
            .extension()
            .and_then(|extension| extension.to_str())
        {
            Some(extension) if extension.eq_ignore_ascii_case("jsonl") => "text/plain".to_string(),
            _ => mime_guess::from_path(&filename)
                .first_or_octet_stream()
                .essence_str()
                .to_string(),
        };
        Ok(Some(FeedbackAttachment {
            filename,
            content_type: Some(content_type),
            buffer,
        }))
    }
}

/// In-memory attachment to include in a feedback upload.
///
/// Use this for generated diagnostics that should not be materialized on disk,
/// such as the redacted doctor report. File-backed artifacts should use
/// `FeedbackAttachmentPath` so upload-time read failures can be logged and
/// skipped independently.
pub struct FeedbackAttachment {
    /// Attachment filename shown in Sentry and in the feedback consent UI.
    pub filename: String,
    /// Optional MIME type for consumers that render or classify attachments.
    pub content_type: Option<String>,
    /// Attachment bytes captured before the upload starts.
    pub buffer: Vec<u8>,
}

/// Inputs that control one feedback upload to Sentry.
///
/// The caller is responsible for applying any user-consent gate before setting
/// `include_logs` or passing diagnostic attachments. This type only describes
/// what to upload once that decision has been made.
pub struct FeedbackUploadOptions<'a> {
    pub classification: &'a str,
    pub reason: Option<&'a str>,
    pub tags: Option<&'a BTreeMap<String, String>>,
    pub include_logs: bool,
    /// Owned diagnostics and frozen rollout prefixes that are safe to upload.
    ///
    /// Diagnostics follow `codex-logs.log`; rollout prefixes join the archive with
    /// path-backed rollouts. Callers must apply any user consent gate first.
    pub extra_attachments: Vec<FeedbackAttachment>,
    pub extra_attachment_paths: &'a [FeedbackAttachmentPath],
    pub session_source: Option<SessionSource>,
    pub logs_override: Option<Vec<u8>>,
}

impl FeedbackSnapshot {
    pub fn refresh_logs(&mut self, feedback: &CodexFeedback) {
        self.bytes = feedback
            .inner
            .ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot_bytes();
    }

    pub fn log_attachment(&self, logs_override: Option<Vec<u8>>) -> FeedbackAttachment {
        FeedbackAttachment {
            filename: "codex-logs.log".to_string(),
            content_type: Some("text/plain".to_string()),
            buffer: logs_override.unwrap_or_else(|| self.bytes.clone()),
        }
    }

    pub fn feedback_diagnostics(&self) -> &FeedbackDiagnostics {
        &self.feedback_diagnostics
    }

    pub fn with_feedback_diagnostics(mut self, feedback_diagnostics: FeedbackDiagnostics) -> Self {
        self.feedback_diagnostics = feedback_diagnostics;
        self
    }

    pub fn feedback_diagnostics_attachment_text(&self, include_logs: bool) -> Option<String> {
        if !include_logs {
            return None;
        }

        self.feedback_diagnostics.attachment_text()
    }

    pub async fn upload_feedback(
        &self,
        _options: FeedbackUploadOptions<'_>,
        _http_client_factory: &HttpClientFactory,
    ) -> Result<()> {
        anyhow::bail!("Feedback uploads are disabled in codex-stripped")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    #[test]
    fn ring_buffer_drops_front_when_full() {
        let fb = CodexFeedback::with_capacity(/*max_bytes*/ 8);
        {
            let mut w = fb.make_writer().make_writer();
            w.write_all(b"abcdefgh").unwrap();
            w.write_all(b"ij").unwrap();
        }
        let snap = fb.snapshot(/*session_id*/ None);
        // Capacity 8: after writing 10 bytes, we should keep the last 8.
        pretty_assertions::assert_eq!(std::str::from_utf8(&snap.bytes).unwrap(), "cdefghij");
    }

    #[test]
    fn logger_layer_filters_noisy_trace_payloads() {
        let fb = CodexFeedback::new();
        let _guard = tracing_subscriber::registry()
            // Keep another TRACE subscriber interested so bridged records are
            // emitted; feedback must still reject them with its own filter.
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::sink))
            .with(fb.logger_layer())
            .set_default();

        tracing::trace!(target: "codex_api::responses_websocket_timing", payload = "secret");
        tracing::event!(
            target: "codex_otel.log_only", tracing::Level::INFO,
            event.name = "codex.agent_response", response = "private-agent-response"
        );
        tracing::info!(target: "codex_otel.log_only", rationale = "private-guardian-rationale");
        tracing::trace!(target: "codex_http_client::transport", "transport-trace");
        tracing::trace!(target: "codex_api::sse", "sse-trace");
        tracing::trace!(target: "codex_api::sse::responses", "nested-sse-trace");
        tracing::debug!(target: "codex_http_client::transport", "transport-debug");
        tracing::debug!(target: "codex_api::sse::responses", "sse-debug");
        tracing::trace!(target: "codex_feedback_test", "unrelated-trace");
        log::trace!(target: "codex_feedback_test", "unrelated-log-trace");
        log::trace!(
            target: "tungstenite::handshake::client",
            "websocket-handshake-payload"
        );
        log::trace!(target: "tungstenite::protocol", "websocket-frame-payload");
        log::debug!(target: "tungstenite::protocol", "websocket-debug");

        let logs = String::from_utf8(fb.snapshot(/*session_id*/ None).bytes).unwrap();
        for excluded in [
            "secret",
            "private-agent-response",
            "private-guardian-rationale",
            "transport-trace",
            "sse-trace",
            "nested-sse-trace",
            "websocket-handshake-payload",
            "websocket-frame-payload",
        ] {
            assert!(!logs.contains(excluded));
        }
        for retained in [
            "transport-debug",
            "sse-debug",
            "unrelated-trace",
            "unrelated-log-trace",
            "websocket-debug",
        ] {
            assert!(logs.contains(retained));
        }
    }

    #[test]
    fn path_backed_attachments_use_binary_content_types() {
        let suffix = ThreadId::new();
        let gzip_filename = format!("codex-desktop-app-logs-{suffix}.tar.gz");
        let unknown_filename = format!("codex-feedback-extra-{suffix}.binunknown");
        let gzip_path = std::env::temp_dir().join(&gzip_filename);
        let unknown_path = std::env::temp_dir().join(&unknown_filename);
        let gzip_bytes = b"\x1f\x8b\x08\x00\xff";
        let unknown_bytes = b"\x00\x9f\x92\x96";
        fs::write(&gzip_path, gzip_bytes).expect("gzip attachment should be written");
        fs::write(&unknown_path, unknown_bytes).expect("unknown attachment should be written");

        let attachments = [gzip_path.clone(), unknown_path.clone()]
            .into_iter()
            .map(|path| {
                FeedbackAttachmentPath {
                    path,
                    attachment_filename_override: None,
                }
                .read_attachment(MAX_ATTACHMENT_BYTES)
                .unwrap()
                .unwrap()
            })
            .collect::<Vec<_>>();

        fs::remove_file(gzip_path).expect("gzip attachment should be removed");
        fs::remove_file(unknown_path).expect("unknown attachment should be removed");
        assert_eq!(
            attachments
                .iter()
                .map(|attachment| (
                    attachment.filename.as_str(),
                    attachment.content_type.as_deref(),
                    attachment.buffer.as_slice(),
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    gzip_filename.as_str(),
                    Some("application/gzip"),
                    gzip_bytes.as_slice(),
                ),
                (
                    unknown_filename.as_str(),
                    Some("application/octet-stream"),
                    unknown_bytes.as_slice(),
                ),
            ]
        );
    }

    #[tokio::test]
    async fn feedback_uploads_are_disabled() {
        let factory =
            HttpClientFactory::new(codex_http_client::OutboundProxyPolicy::ReqwestDefault);
        assert!(FeedbackTransport::new(factory.clone()).is_err());
        let snapshot = CodexFeedback::new().snapshot(None);
        let error = snapshot
            .upload_feedback(
                FeedbackUploadOptions {
                    classification: "bug",
                    reason: None,
                    tags: None,
                    include_logs: true,
                    extra_attachments: Vec::new(),
                    extra_attachment_paths: &[],
                    session_source: None,
                    logs_override: None,
                },
                &factory,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("disabled"));
        assert!(
            snapshot
                .prepare_report_event("any", "bug", None, None, None)
                .is_err()
        );
        assert!(prepare_report_attachment("any", snapshot.log_attachment(None)).is_err());
    }
}
