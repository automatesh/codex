#[test]
fn outbound_requests_do_not_add_trace_headers() {
    let span = tracing::info_span!("outbound-request");
    let _entered = span.enter();
    assert!(super::current_trace_context_headers().is_empty());
}
