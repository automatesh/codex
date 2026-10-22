use super::ExecPolicyManager;
use crate::exec_policy::ExecApprovalRequest;
use crate::sandboxing::SandboxPermissions;
use crate::tools::sandboxing::ExecApprovalRequirement;
use codex_execpolicy::Decision;
use codex_execpolicy::MatchOptions;
use codex_execpolicy::Policy;
use codex_execpolicy::PolicyParser;
use codex_execpolicy::RequirementsExecPolicy;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use pretty_assertions::assert_eq;
use std::sync::Arc;

fn policy_with_broad_allow_prefix() -> (Arc<Policy>, String) {
    let program_name = if cfg!(windows) { "cargo.exe" } else { "cargo" };
    let program_path = std::env::temp_dir()
        .join(program_name)
        .to_string_lossy()
        .into_owned();
    let escaped_program_path = program_path.replace('\\', "\\\\");
    let source = format!(
        r#"
host_executable(name="cargo", paths=["{escaped_program_path}"])
prefix_rule(pattern=["cargo"], decision="allow")
prefix_rule(pattern=["cargo", "publish"], decision="prompt")
prefix_rule(pattern=["rm"], decision="forbidden")
network_rule(host="example.com", protocol="https", decision="allow")
"#
    );
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", &source).expect("parse policy");
    (Arc::new(parser.build()), program_path)
}

#[test]
fn saved_policy_preserves_allow_prompt_deny_network_and_executable_identity() {
    let (policy, program_path) = policy_with_broad_allow_prefix();
    let manager = ExecPolicyManager::new(Arc::clone(&policy));
    let effective = manager.current_for_environment(None);
    assert!(Arc::ptr_eq(&effective, &policy));
    assert_eq!(effective.network_rules(), policy.network_rules());
    assert_eq!(effective.host_executables(), policy.host_executables());
    for (command, expected) in [
        (
            vec!["cargo".to_string(), "install".to_string()],
            Decision::Allow,
        ),
        (
            vec!["cargo".to_string(), "publish".to_string()],
            Decision::Prompt,
        ),
        (vec![program_path, "publish".to_string()], Decision::Prompt),
        (
            vec!["rm".to_string(), "target".to_string()],
            Decision::Forbidden,
        ),
    ] {
        assert_eq!(
            effective
                .check_with_options(
                    &command,
                    &|_| Decision::Prompt,
                    &MatchOptions {
                        resolve_host_executables: true
                    },
                )
                .decision,
            expected,
            "command {command:?}",
        );
    }
}

#[test]
fn environment_restrictions_still_override_saved_allow_rules() {
    let (policy, _) = policy_with_broad_allow_prefix();
    let manager = ExecPolicyManager::new(policy);
    let mut environment_policy = Policy::empty();
    let command = vec!["cargo".to_string(), "install".to_string()];
    environment_policy
        .add_prefix_rule(&command, Decision::Forbidden)
        .expect("add environment restriction");
    let environment_policy = RequirementsExecPolicy::new(environment_policy);
    assert_eq!(
        manager
            .current_for_environment(Some(&environment_policy))
            .check(&command, &|_| Decision::Allow)
            .decision,
        Decision::Forbidden,
    );
}

#[tokio::test]
async fn saved_approvals_apply_to_plain_wrapped_and_resolved_commands() {
    let (policy, program_path) = policy_with_broad_allow_prefix();
    let manager = ExecPolicyManager::new(policy);
    for command in [
        vec![
            "cargo".to_string(),
            "install".to_string(),
            "example".to_string(),
        ],
        vec![
            "bash".to_string(),
            "-lc".to_string(),
            "cargo install example".to_string(),
        ],
        vec![program_path, "install".to_string(), "example".to_string()],
    ] {
        let requirement = manager
            .create_exec_approval_requirement_for_command(ExecApprovalRequest {
                dangerous_commands: codex_instruction_control::DangerousCommands::Confirm,
                command: &command,
                approval_policy: AskForApproval::OnRequest,
                permission_profile: PermissionProfile::read_only(),
                environment_policy: None,
                windows_sandbox_level: WindowsSandboxLevel::Disabled,
                sandbox_permissions: SandboxPermissions::RequireEscalated,
                prefix_rule: None,
            })
            .await;
        assert!(
            matches!(
                requirement,
                ExecApprovalRequirement::Skip {
                    bypass_sandbox: true,
                    ..
                }
            ),
            "saved approval should apply to {command:?}: {requirement:?}"
        );
    }
}
