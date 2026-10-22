pub(crate) fn guardian_truncate_text(content: &str, token_cap: usize) -> (String, bool) {
    (
        codex_guardian_context::truncate_text(content, token_cap),
        content.len() > codex_utils_output_truncation::approx_bytes_for_tokens(token_cap),
    )
}
