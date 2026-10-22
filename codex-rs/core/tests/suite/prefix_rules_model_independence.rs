use anyhow::Result;
use codex_core::config::Config;
use codex_core::config::Constrained;
use codex_core::sandboxing::SandboxPermissions;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::openai_models::MODEL_SPECIALTY_CYBER;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::ThreadSettingsOverrides;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_completed;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_sandbox;
use core_test_support::skip_if_wine_exec;
use core_test_support::submit_thread_settings;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::fs;
use test_case::test_case;

const TEST_COMMAND: &str = "git version";
const SAVED_PREFIX: &str = r#"["git", "version"]"#;

fn configure_saved_prefix(config: &mut Config) {
    let policy_path = config.codex_home.join("rules/default.rules");
    fs::create_dir_all(policy_path.parent().expect("rules directory"))
        .expect("create rules directory");
    fs::write(
        policy_path,
        r#"prefix_rule(pattern=["git", "version"], decision="allow")"#,
    )
    .expect("write saved prefix rule");
    config.permissions.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    config.approvals_reviewer = ApprovalsReviewer::User;
    config
        .set_legacy_sandbox_policy(SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        })
        .expect("set workspace sandbox policy");
}

fn command_response(response_id: &str, call_id: &str) -> Result<String> {
    let args = json!({
        "cmd": TEST_COMMAND,
        "sandbox_permissions": SandboxPermissions::RequireEscalated,
        "justification": "Run the explicitly approved version check.",
        "prefix_rule": ["git", "version"],
    });
    Ok(sse(vec![
        ev_response_created(response_id),
        ev_function_call(call_id, "exec_command", &serde_json::to_string(&args)?),
        ev_completed(response_id),
    ]))
}

async fn submit_model_turn(test: &TestCodex, model: &str) -> Result<()> {
    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            model: Some(model.to_string()),
            approval_policy: Some(AskForApproval::OnRequest),
            approvals_reviewer: Some(ApprovalsReviewer::User),
            ..Default::default()
        },
    )
    .await?;
    test.submit_text_turn("run the approved version check")
        .await
}

#[test_case(false; "general model")]
#[test_case(true; "cyber specialty")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_prefix_is_honored_independently_of_model_specialty(cyber: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    skip_if_wine_exec!(Ok(()), "command execution requires host-native paths");
    let server = start_mock_server().await;
    let mut builder = test_codex()
        .with_model_info_override("gpt-5.4", move |model| {
            model.model_specialty = cyber.then(|| MODEL_SPECIALTY_CYBER.to_string());
        })
        .with_config(configure_saved_prefix);
    let test = builder.build_with_auto_env(&server).await?;
    let responses = mount_sse_sequence(
        &server,
        vec![
            command_response("approved-command", "version-check")?,
            sse_completed("approved-complete"),
        ],
    )
    .await;
    submit_model_turn(&test, "gpt-5.4").await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .message_input_texts("developer")
            .iter()
            .any(|message| message.contains(SAVED_PREFIX))
    );
    assert!(
        responses
            .function_call_output_text("version-check")
            .is_some_and(|output| output.contains(TEST_COMMAND))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_specialties_preserves_saved_approvals() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    skip_if_wine_exec!(Ok(()), "command execution requires host-native paths");
    let server = start_mock_server().await;
    let mut builder = test_codex()
        .with_model_info_override("gpt-5.4", |model| {
            model.model_specialty = Some(MODEL_SPECIALTY_CYBER.to_string());
        })
        .with_model("gpt-5.2")
        .with_config(configure_saved_prefix);
    let test = builder.build_with_auto_env(&server).await?;
    let policy_path = test.codex_home_path().join("rules/default.rules");
    let initial_policy = fs::read_to_string(&policy_path)?;
    let responses = mount_sse_sequence(
        &server,
        vec![
            command_response("general-first", "general-first-command")?,
            sse_completed("general-first-complete"),
            command_response("cyber", "cyber-command")?,
            sse_completed("cyber-complete"),
            command_response("general-last", "general-last-command")?,
            sse_completed("general-last-complete"),
        ],
    )
    .await;
    for model in ["gpt-5.2", "gpt-5.4", "gpt-5.2"] {
        submit_model_turn(&test, model).await?;
    }
    let requests = responses.requests();
    assert_eq!(requests.len(), 6);
    assert!(requests.iter().all(|request| {
        request
            .message_input_texts("developer")
            .iter()
            .any(|message| message.contains(SAVED_PREFIX))
    }));
    for call_id in [
        "general-first-command",
        "cyber-command",
        "general-last-command",
    ] {
        assert!(
            responses
                .function_call_output_text(call_id)
                .is_some_and(|output| output.contains(TEST_COMMAND)),
            "saved approval should execute {call_id}"
        );
    }
    assert_eq!(fs::read_to_string(policy_path)?, initial_policy);
    Ok(())
}
