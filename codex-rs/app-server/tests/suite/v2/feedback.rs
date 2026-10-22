use anyhow::Result;
use app_test_support::TestAppServer;
use codex_app_server_protocol::RequestId;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn stripped_feedback_upload_is_disabled_even_with_explicit_opt_in() -> Result<()> {
    let mut app_server = TestAppServer::builder()
        .with_args(&["-c", "feedback.enabled=true"])
        .build_initialized()
        .await?;
    let request_id = app_server
        .send_raw_request(
            "feedback/upload",
            Some(serde_json::json!({ "classification": "bug", "includeLogs": true })),
        )
        .await?;
    let error = timeout(
        Duration::from_secs(15),
        app_server.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert!(error.error.message.contains("disabled"));
    Ok(())
}
