//! Routes approval requests to the user while preserving mandatory requirements.

use codex_protocol::protocol::ReviewDecision;
use std::sync::Arc;
use tokio::sync::oneshot;

use super::ApprovalRequestReasons;
use super::GuardianReviewContext;
use super::GuardianReviewOptions;
use super::runtime::ReviewAction;
use crate::session::session::Session;

pub(crate) fn spawn_approval_decision(
    session: Arc<Session>,
    context: impl Into<GuardianReviewContext>,
    review_id: String,
    request: impl Into<super::ReviewAction>,
    reasons: ApprovalRequestReasons,
    options: GuardianReviewOptions,
) -> oneshot::Receiver<Option<ReviewDecision>> {
    let context: GuardianReviewContext = context.into();
    let request: ReviewAction = request.into();
    let (tx, rx) = oneshot::channel();
    let runtime = session.services.runtime_handle.clone();
    runtime.spawn(async move {
        let decision =
            decide_approval(session, context, review_id, request, reasons, options).await;
        let _ = tx.send(decision);
    });
    rx
}

/// Use manual approval when no automatic reviewer exists. Mandatory review stays unavailable.
pub(crate) async fn decide_approval(
    session: Arc<Session>,
    context: impl Into<GuardianReviewContext>,
    _review_id: String,
    _request: impl Into<ReviewAction>,
    _reasons: ApprovalRequestReasons,
    options: GuardianReviewOptions,
) -> Option<ReviewDecision> {
    let context = context.into();
    let (_, history_reset) = session.history_reset().await;
    if history_reset.is_cancelled()
        || options
            .external_cancel
            .as_ref()
            .is_some_and(|cancel| cancel.is_cancelled())
    {
        return Some(ReviewDecision::Abort);
    }
    let config = session.get_config().await;
    if history_reset.is_cancelled()
        || options
            .external_cancel
            .as_ref()
            .is_some_and(|cancel| cancel.is_cancelled())
    {
        return Some(ReviewDecision::Abort);
    }
    let requirements = config.config_layer_stack.requirements();
    if options.require_guardian
        || requirements.auto_review_required_for_model(&context.model_info.slug)
        || requirements
            .approvals_reviewer
            .can_set(&codex_protocol::config_types::ApprovalsReviewer::User)
            .is_err()
    {
        return Some(ReviewDecision::denied(
            "Guardian is not included in this build; required automatic review is unavailable",
        ));
    }
    None
}
