use codex_context_fragments::set_annotated_content;
use codex_context_fragments::to_annotated_content;
use codex_history::CompactedItem;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_prompts::ResolvedCollaborationModeMessages;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use std::collections::HashSet;
use std::sync::Arc;

use crate::context::AdditionalContextUserFragment;
use crate::context::ContentFilterGuidance;
use crate::context::ContextualUserFragment;
use crate::context::InternalModelContextFragment;
use crate::context::RecommendedPluginsInstructions;
use crate::context::UserInstructions;
use crate::context::world_state::EnvironmentsState;
use crate::context::world_state::WorldState;
use crate::context::world_state::WorldStateUpdateContent;
use crate::session::ForkPersistence;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use codex_history::InitialHistory;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use serde_json::json;
use tokio_util::sync::CancellationToken;

pub(super) struct ResumeInstructionContext {
    pub(super) fingerprint: Option<String>,
    pub(super) full_history: Option<Arc<Vec<RolloutItem>>>,
    pub(super) history_mode: ThreadHistoryMode,
}

impl Session {
    pub(crate) async fn rebuild_stale_compaction_context(
        &self,
        step: &StepContext,
        world_state: &WorldState,
    ) -> CodexResult<bool> {
        let Some(fingerprint) = self
            .instruction_context_fingerprint(step, world_state)
            .await
        else {
            return Ok(false);
        };
        let expected_history = {
            let state = self.state.lock().await;
            if !state.history.annotated_items().iter().any(|envelope| {
                is_compaction_context(&envelope.item)
                    && envelope
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.instruction_snapshot_id.as_deref())
                        != Some(fingerprint.as_str())
            }) {
                return Ok(false);
            }
            (
                state.history.history_version(),
                state.history.annotated_items().len(),
            )
        };
        let live_thread = self.live_thread().ok_or_else(|| CodexErr::InvalidRequest(
            "The effective instruction context changed after compaction, but the original archive is unavailable. Resume from full original history under the new context; stale instructions were not sent."
                .to_owned(),
        ))?;
        self.flush_rollout().await.map_err(|error| {
            CodexErr::Fatal(format!(
                "Could not flush the original history before rebuilding instructions: {error}"
            ))
        })?;
        let archive = live_thread.load_instruction_history().await.map_err(|error| CodexErr::Fatal(format!(
            "Could not load full original history under the changed instruction context: {error}. No stale context was sent."
        )))?;
        self.apply_instruction_reconstruction(
            &step.turn,
            &archive.items,
            Some(&fingerprint),
            // This view includes the complete frozen lineage, not a paginated suffix.
            ThreadHistoryMode::Legacy,
            Some(expected_history),
        )
        .await?;
        self.services
            .session_extension_data
            .insert(ResumeInstructionContext {
                fingerprint: Some(fingerprint),
                full_history: None,
                history_mode: ThreadHistoryMode::Legacy,
            });
        Ok(true)
    }

    pub(crate) fn compaction_prompt_context_is_stale(
        input: &[ResponseItem],
        metadata: &[Option<codex_history::CodexHarnessMetadata>],
        fingerprint: Option<&str>,
    ) -> bool {
        let Some(fingerprint) = fingerprint else {
            return false;
        };
        input.iter().enumerate().any(|(index, item)| {
            is_compaction_context(item)
                && metadata
                    .get(index)
                    .and_then(Option::as_ref)
                    .and_then(|metadata| metadata.instruction_snapshot_id.as_deref())
                    != Some(fingerprint)
        })
    }

    pub(super) async fn prepare_resume_instruction_context(
        self: &Arc<Self>,
        history: &InitialHistory,
    ) -> CodexResult<()> {
        if matches!(history, InitialHistory::New | InitialHistory::Cleared) {
            return Ok(());
        }
        let turn = self.new_default_turn().await;
        let fingerprint = if turn.config.instruction_snapshot.is_some() {
            let step = self
                .capture_speculative_step_context(Arc::clone(&turn), &CancellationToken::new())
                .await?;
            let world_state = self.build_world_state_for_step(&step, true).await?;
            self.instruction_context_fingerprint(&step, &world_state)
                .await
        } else {
            None
        };
        let validation = rebuild_instruction_history(
            history.get_rollout_items(),
            turn.history_mode,
            fingerprint.as_deref(),
            codex_prompts::ResolvedModelMessages::from_model(turn.model_info())
                .content_filter_guidance(),
        );
        let (full_history, history_mode) = match validation {
            Ok(_) => (None, turn.history_mode),
            Err(original_error) => {
                let archive = match (history, &self.fork_persistence) {
                    (
                        InitialHistory::Forked(items),
                        ForkPersistence::Referenced {
                            history_base: Some(history_base),
                            ..
                        },
                    ) => {
                        let source_id = items
                            .iter()
                            .find_map(|item| match item {
                                RolloutItem::SessionMeta(meta) => Some(meta.meta.id),
                                _ => None,
                            })
                            .ok_or_else(|| {
                                CodexErr::InvalidRequest(format!(
                                    "{original_error} The frozen fork source metadata is missing."
                                ))
                            })?;
                        self.services
                            .thread_store
                            .load_instruction_history_for_fork(
                                codex_thread_store::LoadThreadHistoryParams {
                                    thread_id: source_id,
                                    include_archived: true,
                                },
                                *history_base,
                            )
                            .await
                    }
                    _ => {
                        let Some(live_thread) = self.live_thread() else {
                            return Err(CodexErr::InvalidRequest(original_error));
                        };
                        live_thread.load_instruction_history().await
                    }
                };
                let archive = archive.map_err(|error| {
                    CodexErr::InvalidRequest(format!(
                        "{original_error} Full archive could not be loaded: {error}"
                    ))
                })?;
                if !archive.items.iter().any(|item| {
                    matches!(item, RolloutItem::ResponseItem(envelope) if is_original_context(&envelope.item))
                }) {
                    return Err(CodexErr::InvalidRequest(original_error));
                }
                rebuild_instruction_history(
                    &archive.items,
                    ThreadHistoryMode::Legacy,
                    fingerprint.as_deref(),
                    codex_prompts::ResolvedModelMessages::from_model(turn.model_info())
                        .content_filter_guidance(),
                )
                .map_err(CodexErr::InvalidRequest)?;
                (Some(Arc::new(archive.items)), ThreadHistoryMode::Legacy)
            }
        };
        self.services
            .session_extension_data
            .insert(ResumeInstructionContext {
                fingerprint,
                full_history,
                history_mode,
            });
        Ok(())
    }

    pub(crate) async fn instruction_context_fingerprint(
        &self,
        step: &StepContext,
        world_state: &WorldState,
    ) -> Option<String> {
        let snapshot = step.turn.config.instruction_snapshot.as_ref()?;
        let (world_state_snapshot, updates) = world_state.render_full();
        let rendered = updates
            .into_iter()
            .map(|update| match update.content {
                WorldStateUpdateContent::Fragment(fragment) => fragment.into_boxed_response_item(),
                WorldStateUpdateContent::Item(item) => *item,
            })
            .filter_map(|mut item| {
                let ResponseItem::Message { role, .. } = &item else {
                    return None;
                };
                let developer = role == "developer";
                let mut content = to_annotated_content(&mut item)?;
                content.retain(|fragment| developer || !keep_user_kind(&fragment.kind().0, false));
                (!content.is_empty()).then(|| {
                    content
                        .into_iter()
                        .filter_map(|fragment| {
                            let (content, kind) = fragment.into_parts();
                            fingerprint_fragment(kind.as_str(), content)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .filter(|fragments| !fragments.is_empty())
            .collect::<Vec<_>>();
        let roots = step
            .selected_capability_roots
            .iter()
            .map(|root| root.selected_root())
            .collect::<Vec<_>>();
        let environments = step
            .environments
            .all_selections()
            .into_iter()
            .map(|selection| {
                let config = match &selection.config {
                    codex_protocol::protocol::EnvironmentConfigState::Ready(config) => json!({
                        "state": "ready",
                        "allow_login_shell": config.allow_login_shell,
                        "workspace_roots": config.workspace_roots,
                        "permission_profile": config.permission_profile.permission_profile(),
                        "active_permission_profile": config.permission_profile.active_permission_profile(),
                        "shell_environment_policy": shell_environment_fingerprint(&config.shell_environment_policy),
                        "windows_sandbox_level": config.windows_sandbox_level,
                        "windows_sandbox_type": config.windows_sandbox_type,
                        "use_legacy_landlock": config.use_legacy_landlock,
                        "exec_policy": config.exec_policy.as_ref().map(|policy| policy.fingerprint()),
                        "mcp_policy": config.mcp_policy.as_ref().map(|policy| format!("{policy:?}")),
                        "network_policy": config.network_policy,
                        "selected_capability_roots": config.selected_capability_roots,
                    }),
                    codex_protocol::protocol::EnvironmentConfigState::FromThread => json!({"state": "from_thread"}),
                    codex_protocol::protocol::EnvironmentConfigState::Pending => json!({"state": "pending"}),
                    codex_protocol::protocol::EnvironmentConfigState::Failed(error) => json!({"state": "failed", "error": error}),
                };
                json!({
                    "environment_id": selection.environment_id,
                    "cwd": selection.cwd,
                    "workspace_roots": selection.workspace_roots,
                    "config": config,
                })
            })
            .collect::<Vec<_>>();
        let material = json!({
            "version": 1,
            "base_instructions": self.get_prompt_base_instructions().await.text,
            "developer_instructions": step.turn.developer_instructions,
            "model": step.settings.model_info.as_ref(),
            "collaboration_mode": step.settings.effective_collaboration_mode(),
            "environments": environments,
            "selected_capability_roots": roots,
            "disabled_plugins": step.turn.disabled_plugin_ids,
            "configured_plugins": step.turn.config.plugins,
            "session_source": step.turn.session_source,
            "controls": step.turn.config.fork,
            "world_state": stable_world_state(serde_json::to_value(world_state_snapshot).expect("world state serializes")),
            "rendered_instructions": rendered,
            "tools": step.tool_router.model_visible_specs().as_ref(),
            "token_budget_templates": step.token_budget,
        });
        Some(context_fingerprint(&snapshot.snapshot_id, material))
    }
}

fn is_compaction_context(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
    ) || matches!(item, ResponseItem::Message { role, internal_chat_message_metadata_passthrough: Some(metadata), .. }
            if role == "user" && metadata.content_item_kinds.as_ref().is_some_and(|kinds|
                kinds.iter().any(|kind| kind.as_str() == "compaction.summary")))
}

fn shell_environment_fingerprint(
    policy: &codex_protocol::config_types::ShellEnvironmentPolicy,
) -> serde_json::Value {
    let patterns = |patterns: &[codex_protocol::config_types::EnvironmentVariablePattern]| {
        patterns
            .iter()
            .map(|pattern| {
                json!({
                    "pattern": pattern.pattern(),
                    "case_insensitive": pattern.is_case_insensitive(),
                })
            })
            .collect::<Vec<_>>()
    };
    json!({
        "inherit": policy.inherit,
        "ignore_default_excludes": policy.ignore_default_excludes,
        "exclude": patterns(&policy.exclude),
        "set": policy.r#set,
        "include_only": patterns(&policy.include_only),
        "use_profile": policy.use_profile,
    })
}

pub(super) fn context_key_matches_policy(key: Option<&str>, policy_id: Option<&str>) -> bool {
    match (key, policy_id) {
        (None, None) => true,
        (Some(key), Some(policy_id)) => key
            .strip_prefix(policy_id)
            .and_then(|suffix| suffix.strip_prefix(':'))
            .is_some_and(|digest| {
                digest.len() == 64 && digest.bytes().all(|c| c.is_ascii_hexdigit())
            }),
        _ => false,
    }
}

fn context_fingerprint(policy_id: &str, mut material: serde_json::Value) -> String {
    material.sort_all_objects();
    format!(
        "{policy_id}:{}",
        codex_instruction_control::sha256(material.to_string().as_bytes())
    )
}

fn stable_world_state(mut state: serde_json::Value) -> serde_json::Value {
    if let Some(environment) = state
        .get_mut("environments")
        .and_then(serde_json::Value::as_object_mut)
    {
        environment.remove("current_date");
        environment.remove("subagents");
    }
    state
}

fn fingerprint_fragment(kind: &str, mut content: ContentItem) -> Option<serde_json::Value> {
    if matches!(
        kind,
        "current_time.reminder"
            | "current_time.unavailable"
            | "token_budget.remaining_tokens"
            | "token_budget.reminder"
            | "rollout_budget.remaining_tokens"
            | "environments.environment_context"
    ) {
        return None;
    }
    if kind == "token_budget.context_window"
        && let ContentItem::InputText { text } = &mut content
    {
        *text = text
            .lines()
            .filter(|line| {
                ![
                    "First context window id: ",
                    "Current context window id: ",
                    "Previous context window id: ",
                ]
                .iter()
                .any(|prefix| {
                    line.strip_prefix(prefix)
                        .is_some_and(|value| uuid::Uuid::parse_str(value).is_ok())
                })
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    Some(json!({"kind": kind, "content": content}))
}

pub(super) struct RebuiltInstructionHistory {
    pub(super) items: Vec<RolloutItem>,
}

pub(super) fn rebuild_collaboration_mode(
    saved: &CollaborationMode,
    model: String,
    effort: Option<ReasoningEffort>,
    messages: ResolvedCollaborationModeMessages<'_>,
) -> CollaborationMode {
    let instructions = match saved.mode {
        ModeKind::Default => messages.default.text(),
        ModeKind::Plan => messages.plan.text(),
    };
    saved.with_updates(
        Some(model),
        Some(effort),
        Some(Some(instructions.to_owned())),
    )
}

/// Produces a model-input view only. The source rollout and its archives are never rewritten.
pub(super) fn rebuild_instruction_history(
    items: &[RolloutItem],
    history_mode: ThreadHistoryMode,
    snapshot_id: Option<&str>,
    current_content_filter_guidance: &str,
) -> Result<RebuiltInstructionHistory, String> {
    let accepted_user_text: HashSet<&str> = items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::UserMessage(event)) => Some(event.message.as_str()),
            _ => None,
        })
        .collect();
    let mut rebuilt = RebuiltInstructionHistory {
        items: Vec::with_capacity(items.len()),
    };
    let mut has_original_context = false;
    for item in items {
        match item {
            RolloutItem::WorldState(_) => {}
            RolloutItem::Compacted(compacted) => {
                if checkpoint_matches_snapshot(compacted, snapshot_id) {
                    let mut compacted = compacted.clone();
                    if let Some(history) = compacted.replacement_history.as_mut() {
                        filter_history(
                            history,
                            &accepted_user_text,
                            true,
                            current_content_filter_guidance,
                        )?;
                    }
                    rebuilt.items.push(RolloutItem::Compacted(compacted));
                    has_original_context = true;
                } else if !has_original_context
                    || matches!(history_mode, ThreadHistoryMode::Paginated)
                {
                    return Err(
                        "Cannot rebuild instructions from an unversioned or changed-policy compaction without a complete original rollout. Resume from the full archived history; the stored summary may mix task facts with obsolete instructions and has not been adopted. The archive has not been changed."
                            .to_owned(),
                    );
                }
                // Unversioned summaries cannot prove which instructions they contain.
                // Reconstruct from original records rather than interpreting their prose.
            }
            RolloutItem::ResponseItem(envelope) => {
                let mut history = vec![envelope.clone()];
                filter_history(
                    &mut history,
                    &accepted_user_text,
                    false,
                    current_content_filter_guidance,
                )?;
                if let Some(envelope) = history.pop() {
                    has_original_context |= is_original_context(&envelope.item);
                    rebuilt.items.push(RolloutItem::ResponseItem(envelope));
                }
            }
            RolloutItem::InterAgentCommunication(_) => {
                has_original_context = true;
                rebuilt.items.push(item.clone());
            }
            _ => rebuilt.items.push(item.clone()),
        }
    }
    Ok(rebuilt)
}

fn checkpoint_matches_snapshot(compacted: &CompactedItem, snapshot_id: Option<&str>) -> bool {
    let Some(snapshot_id) = snapshot_id else {
        return false;
    };
    compacted.replacement_history.as_ref().is_some_and(|items| {
        !items.is_empty()
            && items.iter().all(|envelope| {
                envelope
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.instruction_snapshot_id.as_deref())
                    == Some(snapshot_id)
            })
    })
}

fn is_original_context(item: &ResponseItem) -> bool {
    matches!(item, ResponseItem::Message { role, .. } if role == "user" || role == "assistant")
        || matches!(
            item,
            ResponseItem::FunctionCall { .. }
                | ResponseItem::FunctionCallOutput { .. }
                | ResponseItem::CustomToolCall { .. }
                | ResponseItem::CustomToolCallOutput { .. }
        )
}

fn filter_history(
    history: &mut Vec<ResponseItemEnvelope>,
    accepted_user_text: &HashSet<&str>,
    same_snapshot: bool,
    current_content_filter_guidance: &str,
) -> Result<(), String> {
    let mut retained = Vec::with_capacity(history.len());
    for mut envelope in std::mem::take(history) {
        let has_filter_guidance = matches!(&envelope.item,
            ResponseItem::Message { role, internal_chat_message_metadata_passthrough: Some(metadata), .. }
            if role == "developer" && metadata.content_item_kinds.as_ref().is_some_and(|kinds|
                kinds.iter().any(|kind| kind.0 == "generic.content_filter_guidance")))
            || matches!(&envelope.item, ResponseItem::Message { role, content, .. }
                if role == "developer" && content.iter().any(|fragment|
                    matches!(fragment, ContentItem::InputText { text }
                        if ContentFilterGuidance::matches_text(text))));
        if has_filter_guidance {
            let current = ContentFilterGuidance {
                text: current_content_filter_guidance.to_owned(),
            };
            if let ResponseItem::Message {
                content,
                encrypted_content,
                internal_chat_message_metadata_passthrough,
                ..
            } = &mut envelope.item
            {
                *content = vec![ContentItem::InputText {
                    text: current.render(),
                }];
                *encrypted_content = None;
                internal_chat_message_metadata_passthrough
                    .get_or_insert_default()
                    .content_item_kinds = Some(vec![current.content_kind()]);
            }
            if let Some(metadata) = &mut envelope.metadata {
                metadata.mark_retained_sources_incomplete();
            }
            retained.push(envelope);
            continue;
        }
        match &envelope.item {
            ResponseItem::Message { role, .. } if role == "developer" || role == "system" => {
                continue;
            }
            ResponseItem::ConfigurationUpdate { .. }
            | ResponseItem::AdditionalTools { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::ToolSearchOutput { .. } => {
                continue;
            }
            ResponseItem::Compaction { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Reasoning { .. }
                if !same_snapshot =>
            {
                continue;
            }
            ResponseItem::Message { role, .. } if role == "user" => {
                let Some(mut content) = to_annotated_content(&mut envelope.item) else {
                    unreachable!("a user message has content");
                };
                for fragment in &content {
                    if fragment.kind().0 == "unknown"
                        && let ContentItem::InputText { text } = fragment.content()
                        && is_ambiguous_legacy_instruction(text)
                        && !accepted_user_text.contains(text.as_str())
                    {
                        return Err(
                            "Cannot distinguish an unannotated legacy instruction or summary from user-authored text in this history. Explicit migration is required; no content or archive has been deleted."
                                .to_owned(),
                        );
                    }
                }
                content.retain(|fragment| keep_user_kind(&fragment.kind().0, same_snapshot));
                if content.is_empty() {
                    continue;
                }
                set_annotated_content(&mut envelope.item, content)
                    .expect("a retained user message has content");
            }
            _ => {}
        }
        if !same_snapshot
            && envelope
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.compaction_output)
        {
            continue;
        }
        retained.push(envelope);
    }
    *history = retained;
    Ok(())
}

fn keep_user_kind(kind: &str, same_snapshot: bool) -> bool {
    if kind.starts_with("user.") {
        return true;
    }
    if kind == "compaction.summary" {
        return same_snapshot;
    }
    // Preserve user input, tool evidence, and task-bearing agent messages. Instruction blocks
    // have explicit provenance; text resemblance alone must never delete a user's quotation.
    !(kind.ends_with(".instructions")
        || kind.ends_with("_instructions")
        || kind.ends_with(".internal_context")
        || kind.starts_with("guardian.")
        || kind.starts_with("skills.")
        || kind.starts_with("plugins.")
        || kind.starts_with("permissions.")
        || kind.starts_with("token_budget.")
        || kind.starts_with("rollout_budget.")
        || kind.starts_with("additional_content.")
        || matches!(
            kind,
            "hooks.additional_context"
                | "model.base_instructions"
                | "environments.environment_context"
                | "multi_agent.model_catalog"
                | "multi_agent.usage_hint"
                | "generic.content_filter_guidance"
                | "tools.deferred_namespaces"
                | "tools.removed_definition"
                | "compaction.auto_fallback_prompt"
        ))
}

fn is_ambiguous_legacy_instruction(text: &str) -> bool {
    UserInstructions::matches_text(text)
        || EnvironmentsState::matches_text(text)
        || AdditionalContextUserFragment::matches_text(text)
        || InternalModelContextFragment::matches_text(text)
        || RecommendedPluginsInstructions::matches_text(text)
        || codex_skills_extension::is_skill_prompt_fragment(text)
        || crate::compact::is_summary_message(text)
}

#[cfg(test)]
#[path = "resume_instructions_tests.rs"]
mod tests;
