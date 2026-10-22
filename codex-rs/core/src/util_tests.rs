use super::*;
use codex_feedback::FeedbackRequestTags;
use codex_feedback::emit_feedback_request_tags_with_auth_env;
use codex_login::AuthEnvTelemetry;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use tracing::Event;
use tracing::Subscriber;
use tracing::field::Visit;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

#[test]
fn feedback_tags_macro_compiles() {
    #[derive(Debug)]
    struct OnlyDebug;

    feedback_tags!(model = "gpt-5.2", cached = true, debug_only = OnlyDebug);
}

#[derive(Default)]
struct TagCollectorVisitor {
    tags: BTreeMap<String, String>,
}

impl Visit for TagCollectorVisitor {
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.tags
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.tags
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.tags
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

#[derive(Clone)]
struct TagCollectorLayer {
    tags: Arc<Mutex<BTreeMap<String, String>>>,
    event_count: Arc<Mutex<usize>>,
}

impl<S> Layer<S> for TagCollectorLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != "feedback_tags" {
            return;
        }
        let mut visitor = TagCollectorVisitor::default();
        event.record(&mut visitor);
        self.tags.lock().unwrap().extend(visitor.tags);
        *self.event_count.lock().unwrap() += 1;
    }
}

#[test]
fn stripped_feedback_does_not_emit_request_or_auth_fields() {
    let tags = Arc::new(Mutex::new(BTreeMap::new()));
    let event_count = Arc::new(Mutex::new(0));
    let _guard = tracing_subscriber::registry()
        .with(TagCollectorLayer {
            tags: tags.clone(),
            event_count: event_count.clone(),
        })
        .set_default();

    let auth_env = AuthEnvTelemetry {
        openai_api_key_env_present: true,
        codex_api_key_env_present: false,
        codex_api_key_env_enabled: true,
        provider_env_key_name: Some("configured".to_string()),
        provider_env_key_present: Some(true),
        refresh_token_url_override_present: true,
    };

    emit_feedback_request_tags_with_auth_env(
        &FeedbackRequestTags {
            endpoint: "/responses",
            auth_header_attached: true,
            auth_header_name: Some("authorization"),
            auth_mode: Some("chatgpt"),
            auth_retry_after_unauthorized: Some(false),
            auth_recovery_mode: Some("managed"),
            auth_recovery_phase: Some("refresh_token"),
            auth_connection_reused: Some(true),
            auth_request_id: Some("req-123"),
            auth_cf_ray: Some("ray-123"),
            auth_error: Some("missing_authorization_header"),
            auth_error_code: Some("token_expired"),
            auth_recovery_followup_success: Some(true),
            auth_recovery_followup_status: Some(200),
        },
        &auth_env,
    );

    let evaluated = std::cell::Cell::new(false);
    feedback_tags!(
        private = {
            evaluated.set(true);
            "private"
        }
    );
    assert!(!evaluated.get());
    emit_feedback_auth_recovery_tags("managed", "refresh", "success", None, None, None, None);
    assert!(tags.lock().unwrap().is_empty());
    assert_eq!(*event_count.lock().unwrap(), 0);
}

#[test]
fn normalize_thread_name_trims_and_rejects_empty() {
    assert_eq!(normalize_thread_name("   "), None);
    assert_eq!(
        normalize_thread_name("  my thread  "),
        Some("my thread".to_string())
    );
}
