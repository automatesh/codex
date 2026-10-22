pub use codex_async_utils::backoff;
use tracing::error;

/// Compatibility macro; feedback metadata values are not evaluated or recorded.
#[macro_export]
macro_rules! feedback_tags {
    ($( $key:ident = $value:expr ),+ $(,)?) => {
        if false {
            $( let _ = ::tracing::field::debug(&$value); )+
        }
    };
}

pub(crate) fn emit_feedback_auth_recovery_tags(
    _auth_recovery_mode: &str,
    _auth_recovery_phase: &str,
    _auth_recovery_outcome: &str,
    _auth_request_id: Option<&str>,
    _auth_cf_ray: Option<&str>,
    _auth_error: Option<&str>,
    _auth_error_code: Option<&str>,
) {
}

pub(crate) fn error_or_panic(message: impl std::string::ToString) {
    if cfg!(debug_assertions) {
        panic!("{}", message.to_string());
    } else {
        error!("{}", message.to_string());
    }
}

/// Trim a thread name and return `None` if it is empty after trimming.
pub fn normalize_thread_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
#[path = "util_tests.rs"]
mod tests;
