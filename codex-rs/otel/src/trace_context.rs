use codex_protocol::protocol::W3cTraceContext;
use std::collections::BTreeMap;
use tracing::Span;

/// An inert compatibility token; trace context is never collected or propagated.
#[derive(Clone, Debug, Default)]
pub struct Context;
impl Context {
    pub fn new() -> Self {
        Self
    }
    pub fn current() -> Self {
        Self
    }
}

pub fn current_span_w3c_trace_context() -> Option<W3cTraceContext> {
    None
}
pub fn span_w3c_trace_context(_span: &Span) -> Option<W3cTraceContext> {
    None
}
pub fn current_span_trace_id() -> Option<String> {
    None
}
pub fn inject_span_w3c_trace_headers(_span: &Span, _headers: &mut http::HeaderMap) -> bool {
    false
}
pub fn context_from_w3c_trace_context(_trace: &W3cTraceContext) -> Option<Context> {
    None
}
pub fn set_parent_from_w3c_trace_context(_span: &Span, _trace: &W3cTraceContext) -> bool {
    false
}
pub fn set_parent_from_context(_span: &Span, _context: Context) {}
pub fn traceparent_context_from_env() -> Option<Context> {
    None
}

/// Validates configured tracestate members before they are propagated in W3C trace context.
pub fn validate_tracestate_entries(
    entries: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    for (key, fields) in entries {
        validate_tracestate_member(key, fields)?;
    }
    Ok(())
}

pub fn validate_tracestate_member(
    member_key: &str,
    fields: &BTreeMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (_, value) = encode_tracestate_member_fields(member_key, fields)?;
    let valid_char = |byte: u8| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'_' | b'-' | b'*' | b'/')
    };
    let valid_start = |part: &str| {
        part.as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    };
    let valid_key = if let Some((tenant, vendor)) = member_key.split_once('@') {
        valid_start(tenant)
            && valid_start(vendor)
            && vendor.len() <= 13
            && tenant.bytes().all(valid_char)
            && vendor.bytes().all(valid_char)
    } else {
        valid_start(member_key) && member_key.bytes().all(valid_char)
    };
    if !valid_key || member_key.len() > 256 || value.len() > 256 {
        return Err(invalid_tracestate_config(
            "invalid configured tracestate member".into(),
        ));
    }
    Ok(())
}

fn encode_tracestate_member_fields(
    member_key: &str,
    fields: &BTreeMap<String, String>,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    // Configured fields are encoded into one opaque tracestate member value.
    // Validate both the field grammar and the final header value so malformed
    // config cannot produce propagated trace context that downstream W3C
    // extractors reject.
    let mut encoded = Vec::with_capacity(fields.len());
    for (field_key, value) in fields {
        if !is_configured_tracestate_field_key(field_key) {
            return Err(invalid_tracestate_config(format!(
                "invalid configured tracestate field key {member_key}.{field_key}"
            )));
        }
        if !is_configured_tracestate_field_value(value) {
            return Err(invalid_tracestate_config(format!(
                "invalid configured tracestate value for {member_key}.{field_key}"
            )));
        }
        encoded.push(format!("{field_key}:{value}"));
    }
    let value = encoded.join(";");
    if !is_header_safe_tracestate_member_value(&value) {
        return Err(invalid_tracestate_config(format!(
            "invalid configured tracestate value for {member_key}"
        )));
    }
    Ok((member_key.to_string(), value))
}

fn is_configured_tracestate_field_key(field_key: &str) -> bool {
    !field_key.is_empty()
        && field_key
            .bytes()
            .all(|byte| matches!(byte, b'!'..=b'~') && !matches!(byte, b':' | b';' | b',' | b'='))
}

fn is_configured_tracestate_field_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| is_tracestate_member_value_byte(byte) && byte != b';')
}

fn is_header_safe_tracestate_member_value(value: &str) -> bool {
    value.is_empty()
        || (value.bytes().all(is_tracestate_member_value_byte)
            && value.as_bytes().last().is_some_and(|byte| *byte != b' '))
}

fn is_tracestate_member_value_byte(byte: u8) -> bool {
    matches!(byte, b' '..=b'~') && !matches!(byte, b',' | b'=')
}

fn invalid_tracestate_config(message: String) -> Box<dyn std::error::Error> {
    Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_context_is_never_propagated() {
        let trace = W3cTraceContext {
            traceparent: Some("00-11111111111111111111111111111111-2222222222222222-01".into()),
            tracestate: Some("vendor=value".into()),
        };
        let span = tracing::info_span!("request");
        let mut headers = http::HeaderMap::new();
        assert!(context_from_w3c_trace_context(&trace).is_none());
        assert!(!set_parent_from_w3c_trace_context(&span, &trace));
        assert!(current_span_w3c_trace_context().is_none());
        assert!(span_w3c_trace_context(&span).is_none());
        assert!(current_span_trace_id().is_none());
        assert!(traceparent_context_from_env().is_none());
        assert!(!inject_span_w3c_trace_headers(&span, &mut headers));
        assert!(headers.is_empty());
    }
}
