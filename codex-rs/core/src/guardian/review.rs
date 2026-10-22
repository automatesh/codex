use codex_analytics::GuardianApprovalRequestSource;
use codex_core_plugins::PluginCommandAttribution;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::protocol::{
    AskForApproval, InternalSessionSource, SessionSource, SubAgentSource,
};
use tokio_util::sync::CancellationToken;

pub(crate) fn new_guardian_review_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub(crate) fn routes_approval_policy_to_guardian(
    policy: AskForApproval,
    reviewer: ApprovalsReviewer,
) -> bool {
    matches!(
        policy,
        AskForApproval::OnRequest | AskForApproval::Granular(_)
    ) && reviewer == ApprovalsReviewer::AutoReview
}

pub(crate) fn is_basic_session_source(source: &SessionSource) -> bool {
    matches!(
        source,
        SessionSource::Internal(InternalSessionSource::Guardian)
    ) || matches!(source, SessionSource::SubAgent(SubAgentSource::Other(label)) if label == super::GUARDIAN_REVIEWER_NAME)
}

#[derive(Clone)]
pub(crate) struct GuardianReviewOptions {
    pub(crate) require_guardian: bool,
    pub(crate) plugin_attribution_override: Option<PluginCommandAttribution>,
    pub(crate) approval_request_source: GuardianApprovalRequestSource,
    pub(crate) external_cancel: Option<CancellationToken>,
    pub(crate) require_synchronous_review: bool,
}
