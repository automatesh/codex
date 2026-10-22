use super::*;
use codex_history::CodexHarnessMetadata;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use serde_json::json;

#[test]
fn effective_model_project_and_mode_change_compaction_provenance() {
    let original = json!({
        "model": {"slug": "model-a", "model_messages": {"instruction": "accepted"}},
        "environments": [{"cwd": "C:/project-a"}],
        "collaboration_mode": {"mode": "default"},
        "rendered_instructions": ["current project instructions"],
    });
    let fingerprint = context_fingerprint("accepted-policy", original.clone());
    for replacement in [
        json!({"slug": "model-b", "model_messages": {"instruction": "accepted"}}),
        json!({"slug": "model-a", "model_messages": {"instruction": "changed"}}),
    ] {
        let mut changed = original.clone();
        changed["model"] = replacement;
        assert_ne!(context_fingerprint("accepted-policy", changed), fingerprint);
    }
    let mut project = original.clone();
    project["environments"][0]["cwd"] = json!("C:/project-b");
    assert_ne!(context_fingerprint("accepted-policy", project), fingerprint);
    let mut mode = original.clone();
    mode["collaboration_mode"]["mode"] = json!("plan");
    assert_ne!(context_fingerprint("accepted-policy", mode), fingerprint);
    let mut instructions = original.clone();
    instructions["rendered_instructions"] = json!(["different active project instructions"]);
    assert_ne!(
        context_fingerprint("accepted-policy", instructions),
        fingerprint
    );
    assert_ne!(context_fingerprint("new-policy", original), fingerprint);
}

#[test]
fn context_fingerprint_is_canonical_and_requires_current_policy_provenance() {
    let first = serde_json::from_str(r#"{"b":{"d":2,"c":1},"a":0}"#).unwrap();
    let second = serde_json::from_str(r#"{"a":0,"b":{"c":1,"d":2}}"#).unwrap();
    let key = context_fingerprint("accepted-policy", first);
    assert_eq!(key, context_fingerprint("accepted-policy", second));
    assert!(context_key_matches_policy(
        Some(&key),
        Some("accepted-policy")
    ));
    assert!(!context_key_matches_policy(
        Some(&key),
        Some("different-policy")
    ));
    assert!(!context_key_matches_policy(
        Some("accepted-policy"),
        Some("accepted-policy")
    ));
    assert!(!context_key_matches_policy(Some(&key), None));
    assert!(!context_key_matches_policy(None, Some("accepted-policy")));
}

#[test]
fn clock_countdowns_and_window_identifiers_are_not_instruction_changes() {
    let window = |id: uuid::Uuid, hint: &str| ContentItem::InputText {
        text: format!(
            "<context_window>\nAgent name: /root\nFirst context window id: {id}\nCurrent context window id: {id}\n{hint}\n</context_window>"
        ),
    };
    let first = fingerprint_fragment(
        "token_budget.context_window",
        window(uuid::Uuid::now_v7(), "retain the notes hint"),
    );
    let second = fingerprint_fragment(
        "token_budget.context_window",
        window(uuid::Uuid::now_v7(), "retain the notes hint"),
    );
    assert_eq!(first, second);
    assert_ne!(
        first,
        fingerprint_fragment(
            "token_budget.context_window",
            window(uuid::Uuid::now_v7(), "changed hint instruction"),
        )
    );
    for kind in [
        "current_time.reminder",
        "token_budget.remaining_tokens",
        "token_budget.reminder",
    ] {
        assert!(
            fingerprint_fragment(
                kind,
                ContentItem::InputText {
                    text: "updated value".into()
                }
            )
            .is_none()
        );
    }
    assert_eq!(
        stable_world_state(
            json!({"environments":{"current_date":"today","subagents":"/root/a","timezone":"UTC"}})
        ),
        stable_world_state(
            json!({"environments":{"current_date":"tomorrow","subagents":"/root/b","timezone":"UTC"}})
        ),
    );
}

#[test]
fn shell_environment_fingerprint_is_stable_and_detects_policy_changes() {
    use codex_protocol::config_types::EnvironmentVariablePattern;
    use codex_protocol::config_types::ShellEnvironmentPolicy;

    let mut first = ShellEnvironmentPolicy::default();
    first.r#set.insert("FIRST".into(), "one".into());
    first.r#set.insert("SECOND".into(), "two".into());
    first
        .exclude
        .push(EnvironmentVariablePattern::new("*SECRET*"));
    let mut second = ShellEnvironmentPolicy::default();
    second.r#set.insert("SECOND".into(), "two".into());
    second.r#set.insert("FIRST".into(), "one".into());
    second.exclude = first.exclude.clone();
    let fingerprint = |policy: &ShellEnvironmentPolicy| {
        context_fingerprint("accepted", shell_environment_fingerprint(policy))
    };
    assert_eq!(fingerprint(&first), fingerprint(&second));
    second.use_profile = !second.use_profile;
    assert_ne!(fingerprint(&first), fingerprint(&second));
    second.use_profile = first.use_profile;
    second.exclude = vec![EnvironmentVariablePattern::new_case_insensitive("*SECRET*")];
    assert_ne!(fingerprint(&first), fingerprint(&second));
}

#[tokio::test]
async fn repeated_effective_context_fingerprints_survive_a_new_window() {
    use codex_instruction_control::ForkConfig;
    use codex_instruction_control::PolicySnapshot;
    use codex_instruction_control::PolicySource;
    use codex_instruction_control::SourceKind;
    use std::sync::Arc;

    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    let snapshot = PolicySnapshot::new(
        ForkConfig::default(),
        vec![
            PolicySource::new(SourceKind::MainPrompt, "main", "accepted main".into()),
            PolicySource::new(
                SourceKind::ModelCatalog,
                "models",
                r#"{"models":[{"slug":"test","model_messages":{}}]}"#.into(),
            ),
        ],
    )
    .unwrap();
    Arc::make_mut(&mut turn.config).instruction_snapshot = Some(Arc::new(snapshot));
    let step = StepContext::for_test(Arc::new(turn));
    let before_world_state = session
        .build_world_state_for_step(&step, true)
        .await
        .unwrap();
    let before = session
        .instruction_context_fingerprint(&step, &before_world_state)
        .await;
    assert!(before.is_some());
    session.advance_auto_compact_window().await;
    let after_world_state = session
        .build_world_state_for_step(&step, true)
        .await
        .unwrap();
    let after = session
        .instruction_context_fingerprint(&step, &after_world_state)
        .await;
    assert_eq!(before, after);
}

#[tokio::test]
async fn live_instruction_change_rebuilds_from_full_archive_without_rewriting_it() {
    use crate::context_manager::HistoryReplacement;
    use codex_instruction_control::ForkConfig;
    use codex_instruction_control::PolicySnapshot;
    use codex_instruction_control::PolicySource;
    use codex_instruction_control::SourceKind;
    use std::sync::Arc;

    let (mut session, mut turn) = crate::session::tests::make_session_and_context().await;
    crate::session::tests::attach_in_memory_thread_store(&mut session).await;
    let task = message("user", "preserve this task", Some("user.text"));
    let call: ResponseItem = serde_json::from_value(json!({
        "type": "function_call", "call_id": "original", "name": "exec_command", "arguments": "{}"
    }))
    .unwrap();
    let output: ResponseItem = serde_json::from_value(json!({
        "type": "function_call_output", "call_id": "original", "output": "original tool result"
    }))
    .unwrap();
    session
        .record_conversation_items(
            &turn,
            turn.model_info(),
            &[task.item, call.clone(), output.clone()],
        )
        .await;
    let original_items = session
        .clone_history()
        .await
        .raw_items()
        .cloned()
        .collect::<Vec<_>>();
    let mut summary = message(
        "user",
        "obsolete summary instructions",
        Some("compaction.summary"),
    );
    summary.metadata = Some(CodexHarnessMetadata {
        instruction_snapshot_id: Some("different-context".into()),
        ..Default::default()
    });
    session
        .persist_rollout_items(&[checkpoint(vec![summary.clone()])])
        .await;
    session.state.lock().await.replace_annotated_history(
        vec![summary],
        None,
        HistoryReplacement::Reset,
    );
    let before_archive = session
        .live_thread()
        .unwrap()
        .load_history(true)
        .await
        .unwrap();
    let before_archive = serde_json::to_value(before_archive.items).unwrap();
    let snapshot = PolicySnapshot::new(
        ForkConfig::default(),
        vec![
            PolicySource::new(SourceKind::MainPrompt, "main", "accepted main".into()),
            PolicySource::new(
                SourceKind::ModelCatalog,
                "models",
                r#"{"models":[{"slug":"test","model_messages":{}}]}"#.into(),
            ),
        ],
    )
    .unwrap();
    Arc::make_mut(&mut turn.config).instruction_snapshot = Some(Arc::new(snapshot));
    let step = StepContext::for_test(Arc::new(turn));
    let world_state = session
        .build_world_state_for_step(&step, true)
        .await
        .unwrap();
    assert!(
        session
            .rebuild_stale_compaction_context(&step, &world_state)
            .await
            .unwrap()
    );
    let history = session.clone_history().await;
    for original in original_items {
        assert!(history.raw_items().any(|item| item == &original));
    }
    assert!(!history.raw_items().any(is_compaction_context));
    assert!(session.reference_context_item().await.is_none());
    let after_archive = session
        .live_thread()
        .unwrap()
        .load_history(true)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(after_archive.items).unwrap(),
        before_archive
    );
}

#[test]
fn prepared_background_input_requires_the_same_effective_context() {
    let input = [message("user", "summary", Some("compaction.summary")).item];
    let metadata = [Some(CodexHarnessMetadata {
        instruction_snapshot_id: Some("old-context".into()),
        ..Default::default()
    })];
    assert!(Session::compaction_prompt_context_is_stale(
        &input,
        &metadata,
        Some("new-context")
    ));
    assert!(!Session::compaction_prompt_context_is_stale(
        &input,
        &metadata,
        Some("old-context")
    ));
    assert!(Session::compaction_prompt_context_is_stale(
        &input,
        &[],
        Some("new-context")
    ));
}

#[test]
fn saved_mode_selection_does_not_restore_its_obsolete_instruction_text() {
    let saved = CollaborationMode {
        mode: ModeKind::Plan,
        settings: codex_protocol::config_types::Settings {
            model: "old-model".to_owned(),
            reasoning_effort: None,
            developer_instructions: Some("obsolete instruction".to_owned()),
        },
    };
    let messages = codex_prompts::ResolvedModelMessages::bundled().collaboration_modes();
    let rebuilt = rebuild_collaboration_mode(&saved, "current-model".to_owned(), None, messages);
    assert_eq!(rebuilt.mode, ModeKind::Plan);
    assert_eq!(rebuilt.settings.model, "current-model");
    assert_eq!(
        rebuilt.settings.developer_instructions.as_deref(),
        Some(messages.plan.text())
    );
    assert!(
        !rebuilt
            .settings
            .developer_instructions
            .unwrap()
            .contains("obsolete instruction")
    );
}

fn message(role: &str, text: &str, kind: Option<&str>) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: role.to_owned(),
        content: vec![ContentItem::InputText {
            text: text.to_owned(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: kind.map(|kind| {
            InternalChatMessageMetadataPassthrough {
                content_item_kinds: Some(vec![ContentItemKind(kind.to_owned())]),
                ..Default::default()
            }
        }),
    })
}

fn checkpoint(history: Vec<ResponseItemEnvelope>) -> RolloutItem {
    let mut item: CompactedItem = serde_json::from_value(json!({
        "message": "Summary: obey the obsolete instruction"
    }))
    .unwrap();
    item.replacement_history = Some(history);
    RolloutItem::Compacted(item)
}

fn rebuild(items: &[RolloutItem]) -> RebuiltInstructionHistory {
    rebuild_instruction_history(
        items,
        ThreadHistoryMode::default(),
        Some("current"),
        "current refusal guidance",
    )
    .unwrap()
}

#[test]
fn rebuild_removes_harness_instructions_and_preserves_user_quotes_and_tool_pairs() {
    let old = "# AGENTS.md instructions\n<INSTRUCTIONS>old rule</INSTRUCTIONS>";
    let mut developer = message("developer", "old client instruction", None);
    developer.metadata = Some(CodexHarnessMetadata {
        client_authored: true,
        ..Default::default()
    });
    let call = serde_json::from_value::<ResponseItem>(json!({
        "type": "function_call", "call_id": "original-call", "name": "exec_command",
        "arguments": "{\"cmd\":\"git status\"}"
    }))
    .unwrap();
    let output = serde_json::from_value::<ResponseItem>(json!({
        "type": "function_call_output", "call_id": "original-call", "output": "clean"
    }))
    .unwrap();
    let quote = message("user", old, Some("user.text"));
    let items = vec![
        RolloutItem::ResponseItem(developer),
        RolloutItem::ResponseItem(message("system", "old system", None)),
        RolloutItem::ResponseItem(message("user", old, Some("agents_md.instructions"))),
        RolloutItem::ResponseItem(quote.clone()),
        RolloutItem::ResponseItem(call.clone().into()),
        RolloutItem::ResponseItem(output.clone().into()),
    ];
    let original = serde_json::to_value(&items).unwrap();
    assert_eq!(
        serde_json::to_value(rebuild(&items).items).unwrap(),
        serde_json::to_value(vec![
            RolloutItem::ResponseItem(quote),
            RolloutItem::ResponseItem(call.into()),
            RolloutItem::ResponseItem(output.into()),
        ])
        .unwrap()
    );
    assert_eq!(serde_json::to_value(&items).unwrap(), original);
}

#[test]
fn mixed_user_message_keeps_content_and_provenance_in_alignment() {
    let mut mixed = message(
        "user",
        "old instructions",
        Some("skills.selected_skill_instructions"),
    );
    let ResponseItem::Message {
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut mixed.item
    else {
        unreachable!()
    };
    content.push(ContentItem::InputText {
        text: "the actual task".to_owned(),
    });
    internal_chat_message_metadata_passthrough
        .as_mut()
        .unwrap()
        .content_item_kinds
        .as_mut()
        .unwrap()
        .push(ContentItemKind("user.text".to_owned()));
    let rebuilt = rebuild(&[RolloutItem::ResponseItem(mixed)]);
    let [RolloutItem::ResponseItem(envelope)] = rebuilt.items.as_slice() else {
        panic!("only the user task must survive")
    };
    assert_eq!(
        envelope,
        &message("user", "the actual task", Some("user.text"))
    );
}

#[test]
fn legacy_wrapper_requires_provenance_without_deleting_user_content() {
    let text = "# AGENTS.md instructions\n<INSTRUCTIONS>quoted by the user</INSTRUCTIONS>";
    let input = RolloutItem::ResponseItem(message("user", text, None));
    let original = serde_json::to_value(&input).unwrap();
    assert!(
        rebuild_instruction_history(
            std::slice::from_ref(&input),
            ThreadHistoryMode::default(),
            Some("current"),
            "current refusal guidance",
        )
        .is_err()
    );
    assert_eq!(serde_json::to_value(&input).unwrap(), original);
    let accepted = RolloutItem::EventMsg(EventMsg::UserMessage(
        codex_protocol::protocol::UserMessageEvent {
            message: text.to_owned(),
            ..Default::default()
        },
    ));
    let rebuilt = rebuild(&[accepted, input]);
    assert_eq!(rebuilt.items.len(), 2);
}

#[test]
fn unversioned_summary_is_rebuilt_from_original_task_records() {
    let user = RolloutItem::ResponseItem(message("user", "implement the task", Some("user.text")));
    let result = RolloutItem::ResponseItem(message("assistant", "implemented", None));
    let summary = checkpoint(vec![message(
        "user",
        "old instruction mixed with task facts",
        Some("compaction.summary"),
    )]);
    let items = vec![user.clone(), result.clone(), summary];
    let original = serde_json::to_value(&items).unwrap();
    assert_eq!(
        serde_json::to_value(rebuild(&items).items).unwrap(),
        serde_json::to_value(vec![user, result]).unwrap()
    );
    assert_eq!(serde_json::to_value(&items).unwrap(), original);
}

#[test]
fn accepted_checkpoint_keeps_summary_but_regenerates_instruction_blocks() {
    let mut history = vec![
        message(
            "developer",
            "obsolete rendering of current instructions",
            None,
        ),
        message("user", "task facts", Some("compaction.summary")),
    ];
    for envelope in &mut history {
        envelope.metadata = Some(CodexHarnessMetadata {
            instruction_snapshot_id: Some("current".to_owned()),
            ..Default::default()
        });
    }
    let rebuilt = rebuild(&[checkpoint(history)]);
    let [RolloutItem::Compacted(compacted)] = rebuilt.items.as_slice() else {
        panic!("the current-snapshot checkpoint must remain usable")
    };
    let history = compacted.replacement_history.as_ref().unwrap();
    assert_eq!(history.len(), 1);
    assert!(matches!(&history[0].item, ResponseItem::Message { role, .. } if role == "user"));
}

#[test]
fn changed_snapshot_and_missing_originals_require_explicit_migration() {
    let mut summary = message("user", "mixed summary", Some("compaction.summary"));
    summary.metadata = Some(CodexHarnessMetadata {
        instruction_snapshot_id: Some("old".to_owned()),
        ..Default::default()
    });
    let checkpoint = checkpoint(vec![summary]);
    assert!(
        rebuild_instruction_history(
            std::slice::from_ref(&checkpoint),
            ThreadHistoryMode::default(),
            Some("current"),
            "current refusal guidance",
        )
        .is_err()
    );
    assert!(
        rebuild_instruction_history(
            &[
                RolloutItem::ResponseItem(message("user", "tail only", Some("user.text"))),
                checkpoint,
            ],
            ThreadHistoryMode::Paginated,
            Some("current"),
            "current refusal guidance",
        )
        .is_err()
    );
}

#[test]
fn content_filter_guidance_is_regenerated_from_current_policy() {
    let items = [RolloutItem::ResponseItem(message(
        "developer",
        "old guidance wording",
        Some("generic.content_filter_guidance"),
    ))];
    let rebuilt = rebuild(&items);
    let [RolloutItem::ResponseItem(envelope)] = rebuilt.items.as_slice() else {
        panic!("current service policy must retain its chronological position")
    };
    assert!(
        matches!(&envelope.item, ResponseItem::Message { role, content, .. }
        if role == "developer" && matches!(&content[..], [ContentItem::InputText { text }]
            if text.contains("current refusal guidance") && !text.contains("old guidance wording")))
    );
}

#[test]
fn snapshot_metadata_is_backward_compatible_and_round_trips() {
    let old: CodexHarnessMetadata = serde_json::from_value(json!({})).unwrap();
    assert!(old.instruction_snapshot_id.is_none());
    let current = CodexHarnessMetadata {
        instruction_snapshot_id: Some("accepted".to_owned()),
        ..Default::default()
    };
    assert_eq!(
        serde_json::from_value::<CodexHarnessMetadata>(serde_json::to_value(&current).unwrap())
            .unwrap(),
        current
    );
}

#[tokio::test]
async fn resume_and_fork_invalidate_the_old_context_baseline() {
    use crate::session::tests::make_session_and_context;
    use codex_history::InitialHistory;
    use codex_history::ResumedHistory;
    use std::sync::Arc;

    for fork in [false, true] {
        let (session, turn) = make_session_and_context().await;
        let items = vec![
            RolloutItem::TurnContext(turn.to_turn_context_item()),
            RolloutItem::ResponseItem(message("developer", "old instruction", None)),
            RolloutItem::ResponseItem(message("user", "continue my task", Some("user.text"))),
        ];
        let initial = if fork {
            InitialHistory::Forked(items)
        } else {
            InitialHistory::Resumed(ResumedHistory {
                history_revision: None,
                conversation_id: session.thread_id(),
                history: Arc::new(items),
                rollout_path: None,
            })
        };
        session.record_initial_history(initial).await;
        assert!(session.reference_context_item().await.is_none());
        assert!(session.clone_history().await.raw_items().any(|item| {
            matches!(item, ResponseItem::Message { role, content, .. }
                if role == "user" && content.iter().any(|part|
                    matches!(part, ContentItem::InputText { text } if text == "continue my task")))
        }));
        assert!(!session.clone_history().await.raw_items().any(|item| {
            matches!(item, ResponseItem::Message { content, .. }
                if content.iter().any(|part|
                    matches!(part, ContentItem::InputText { text } if text == "old instruction")))
        }));
    }
}

#[tokio::test]
async fn rebuild_does_not_resurrect_service_guidance_from_a_rolled_back_turn() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let items = vec![
        RolloutItem::ResponseItem(message("user", "discard this task", Some("user.text"))),
        RolloutItem::ResponseItem(message(
            "developer",
            "old refusal wording",
            Some("generic.content_filter_guidance"),
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
        RolloutItem::ResponseItem(message("user", "the surviving task", Some("user.text"))),
    ];
    let rebuilt = rebuild(&items);
    let reconstructed = session
        .reconstruct_history_from_rollout(&turn, &rebuilt.items)
        .await;
    assert_eq!(reconstructed.history.len(), 1);
    assert!(matches!(&reconstructed.history[0].item,
        ResponseItem::Message { role, content, .. }
        if role == "user" && matches!(&content[..], [ContentItem::InputText { text }]
            if text == "the surviving task")));
}

#[tokio::test]
async fn compaction_from_a_different_instruction_snapshot_cannot_replace_live_history() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    session
        .record_conversation_items(
            &turn,
            turn.model_info(),
            &[message("user", "the live task", Some("user.text")).item],
        )
        .await;
    let before = session.clone_history().await;
    let (window_number, window_ids) = session.advance_auto_compact_window().await;
    let installed = session
        .replace_compacted_history(
            vec![message("user", "stale summary", Some("compaction.summary"))],
            turn.to_turn_context_item(),
            crate::context::world_state::WorldStateSnapshot::default(),
            crate::compact::CompactedHistoryMetadata {
                instruction_snapshot_id: Some("different-snapshot".to_owned()),
                input_goal_ids: Default::default(),
                message: String::new(),
                window_number,
                window_ids,
                compaction_response_id: None,
                compaction_model_hash: None,
                reviewer_compaction_hash: None,
            },
        )
        .await;
    assert!(!installed);
    assert_eq!(
        session.clone_history().await.annotated_items(),
        before.annotated_items()
    );
}
