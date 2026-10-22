//! Approval request compatibility and manual approval routing.

mod approval_request;
mod coverage;
mod decision;
mod input_budget;
mod permissions;
mod prompt;
mod request_budget;
mod review;
mod runtime;
pub(crate) use input_budget::PendingReviewContext;
pub(crate) use input_budget::check_pending as check_pending_guardian_input;
pub(crate) use input_budget::finalize as finalize_guardian_input;
pub(crate) use input_budget::should_compact as should_compact_guardian_input;
pub(crate) use permissions::for_tool as tool_permission_context;
pub(crate) use request_budget::ExhaustedReviewBudget;
pub(crate) use request_budget::observe as observe_guardian_request;
pub(crate) use request_budget::prepare_prompt as prepare_guardian_prompt;
pub(crate) use runtime::ReviewAction;

use codex_protocol::items::ModelInvocationContext;
use std::sync::Arc;

use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;

use crate::agents_md::LoadedAgentsMd;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::step_context::StepContext;
use crate::session::step_settings::ResolvedStepSettings;
use crate::session::turn_context::TurnContext;
use crate::tools::sandboxing::ApprovalRequestReasons;

pub(crate) use approval_request::GuardianApprovalRequest;
pub(crate) use approval_request::GuardianMcpAnnotations;
pub(crate) use approval_request::GuardianNetworkAccessTrigger;
#[cfg(test)]
pub(crate) use approval_request::guardian_approval_request_to_json;
pub(crate) use decision::decide_approval;
pub(crate) use decision::spawn_approval_decision;
pub(crate) use prompt::guardian_truncate_text;
pub(crate) use review::GuardianReviewOptions;
pub(crate) use review::is_basic_session_source;
pub(crate) use review::new_guardian_review_id;
pub(crate) use review::routes_approval_policy_to_guardian;
pub(crate) const GUARDIAN_REVIEWER_NAME: &str = "guardian";
pub(crate) const AUTO_REVIEW_DENIED_ACTION_APPROVAL_DEVELOPER_PREFIX: &str =
    codex_guardian_context::MANUAL_APPROVAL_DEVELOPER_PREFIX;
pub(crate) const GUARDIAN_MAX_ROOT_MESSAGE_TOKENS: usize = 900;
pub(crate) const GUARDIAN_MAX_NODE_REPL_TOOL_RESULT_TOKENS: usize = 6_000;

/// Captures review inputs from the issuing step without retaining its MCP bindings or tool router.
/// Background network approvals and Unix interception use the active task's resolved settings.
/// Startup reviewer prewarming intentionally uses turn-only inputs because it has no issuing step.
///
/// MCP elicitations use their live invocation's issuing step when available, otherwise turn inputs.
#[derive(Clone)]
pub(crate) struct GuardianReviewContext {
    /// The latest response ID received in this turn when review was requested.
    pub(crate) parent_response_id: Option<String>,
    turn: Arc<TurnContext>,
    environments: TurnEnvironmentSnapshot,
    project_instructions: Option<Arc<LoadedAgentsMd>>,
    // Model and reasoning inputs are carried for the follow-up Guardian and V2 migrations.
    pub(crate) model_info: Arc<ModelInfo>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) reasoning_summary: ReasoningSummary,
    pub(crate) personality: Option<Personality>,
    pub(crate) approval_policy: AskForApproval,
    pub(crate) approvals_reviewer: ApprovalsReviewer,
}

impl GuardianReviewContext {
    /// Keep the issuing instructions across retries even if a later turn selects other environments.
    async fn capture_project_instructions(&mut self, session: &crate::session::session::Session) {
        if self.project_instructions.is_none() {
            self.project_instructions = session
                .services
                .agents_md_manager
                .project_snapshot(&self.turn.config, &self.environments)
                .await;
        }
    }

    pub(crate) fn model_context(&self) -> ModelInvocationContext {
        ModelInvocationContext {
            model_slug: self.model_info.slug.clone(),
            reasoning_effort: self
                .reasoning_effort
                .as_ref()
                .or(self.model_info.default_reasoning_level.as_ref())
                .map(ToString::to_string),
        }
    }

    pub(crate) fn from_resolved_settings(
        turn: Arc<TurnContext>,
        settings: &ResolvedStepSettings,
        environments: &TurnEnvironmentSnapshot,
    ) -> Self {
        Self {
            parent_response_id: turn
                .extension_data
                .get::<codex_api::ResponseId>()
                .map(|id| id.0.clone()),
            environments: environments.clone(),
            project_instructions: None,
            model_info: Arc::clone(&settings.model_info),
            reasoning_effort: settings.reasoning_effort().cloned(),
            reasoning_summary: settings.reasoning_summary,
            personality: settings.personality(),
            approval_policy: settings.approval_policy(),
            approvals_reviewer: settings.approvals_reviewer(),
            turn,
        }
    }

    pub(crate) fn turn(&self) -> &Arc<TurnContext> {
        &self.turn
    }

    pub(crate) fn environments(&self) -> &TurnEnvironmentSnapshot {
        &self.environments
    }
}

impl From<&Arc<StepContext>> for GuardianReviewContext {
    fn from(step: &Arc<StepContext>) -> Self {
        Self {
            parent_response_id: step
                .turn
                .extension_data
                .get::<codex_api::ResponseId>()
                .map(|id| id.0.clone()),
            turn: Arc::clone(&step.turn),
            environments: step.environments.clone(),
            project_instructions: Some(step.loaded_agents_md.clone().unwrap_or_default()),
            model_info: Arc::clone(&step.settings.model_info),
            reasoning_effort: step.settings.reasoning_effort().cloned(),
            reasoning_summary: step.settings.reasoning_summary,
            personality: step.settings.personality(),
            approval_policy: step.settings.approval_policy(),
            approvals_reviewer: step.settings.approvals_reviewer(),
        }
    }
}

impl From<Arc<TurnContext>> for GuardianReviewContext {
    fn from(turn: Arc<TurnContext>) -> Self {
        Self {
            parent_response_id: turn
                .extension_data
                .get::<codex_api::ResponseId>()
                .map(|id| id.0.clone()),
            environments: turn.initial_environments.clone(),
            project_instructions: None,
            model_info: Arc::clone(turn.model_info()),
            reasoning_effort: turn.reasoning_effort().cloned(),
            reasoning_summary: turn.reasoning_summary(),
            personality: turn.personality(),
            approval_policy: turn.approval_policy(),
            approvals_reviewer: turn.config.approvals_reviewer,
            turn,
        }
    }
}

impl From<&Arc<TurnContext>> for GuardianReviewContext {
    fn from(turn: &Arc<TurnContext>) -> Self {
        Self::from(Arc::clone(turn))
    }
}

pub(crate) use approval_request::format_guardian_action_pretty;
pub(crate) const GUARDIAN_REVIEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

pub(crate) fn reviewer_tool_policy() -> codex_extension_api::ToolPolicy {
    codex_extension_api::ToolPolicy {
        allowed_tools: Some(
            ["exec_command", "write_stdin", "view_image", "exec", "wait"]
                .into_iter()
                .map(codex_extension_api::ToolName::plain)
                .collect(),
        ),
        require_managed_sandbox: true,
        require_unified_exec: true,
        expose_additional_permissions: false,
    }
}
