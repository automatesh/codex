use crate::Prompt;
use crate::config::Config;
use anyhow::Result;
use codex_context_fragments::to_annotated_content;
use codex_instruction_control::PolicySnapshot;
use codex_instruction_control::PolicySource;
use codex_instruction_control::SourceKind;
use codex_instruction_control::sha256;
use codex_protocol::error::CodexErr;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use std::collections::BTreeMap;
use std::io::Write;

mod model_switch;

fn dynamic_state(kind: &str) -> bool {
    matches!(
        kind,
        "permissions.instructions"
            | "permissions.compact"
            | "permissions.approved_command_prefix_saved"
            | "permissions.network_rule_saved"
            | "environment.context"
            | "environments.state"
            | "generic.current_time"
            | "token_budget.remaining_tokens"
            | "token_budget.reminder"
            | "token_budget.context_window"
            | "rollout_budget.remaining_tokens"
            | "skills.catalog"
            | "environments.environment_context"
            | "current_time.reminder"
            | "current_time.unavailable"
            | "network_proxy.rule_saved"
            | "user.instructions"
            | "agents_md.instructions"
    )
}

fn canonical_content(text: &str) -> String {
    let Some(snapshot) = codex_instruction_control::runtime_snapshot() else {
        return text.to_owned();
    };
    let Ok(root) = codex_instruction_control::protected_install_dir() else {
        return text.to_owned();
    };
    let root = root
        .join("Policy")
        .join("sources")
        .to_string_lossy()
        .to_string();
    canonical_source_paths(text, &root, &snapshot.snapshot_id)
}

fn canonical_source_paths(text: &str, root: &str, active_id: &str) -> String {
    let mut result = text.to_owned();
    let native = format!("{root}\\");
    let forward = format!("{}/", root.replace('\\', "/"));
    let escaped = native.replace('\\', "\\\\");
    for prefix in [native, forward, escaped] {
        let mut replacements = Vec::new();
        for (start, _) in result.match_indices(&prefix) {
            let index = start + prefix.len();
            let bytes = result.as_bytes();
            let boundary = start == 0
                || matches!(
                    bytes[start - 1],
                    b' ' | b'\n' | b'\r' | b'\t' | b'"' | b'\'' | b'`' | b'<' | b'('
                );
            if boundary
                && bytes
                    .get(index..index + 64)
                    .is_some_and(|hash| hash == active_id.as_bytes())
                && bytes
                    .get(index + 64)
                    .is_some_and(|separator| matches!(separator, b'/' | b'\\'))
            {
                replacements.push(index);
            }
        }
        for index in replacements.into_iter().rev() {
            result.replace_range(index..index + 64, "ACCEPTED_SNAPSHOT");
        }
    }
    result
}

fn capture_raw_tool_sources(
    tools: &[serde_json::Value],
    prefix: &str,
) -> Result<Vec<PolicySource>> {
    let mut sources = Vec::new();
    for tool in tools {
        let name = tool
            .get("name")
            .or_else(|| tool.get("type"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("tool declaration is missing its name and type"))?;
        if tool.get("type").and_then(serde_json::Value::as_str) == Some("namespace") {
            let children = tool
                .get("tools")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("namespace {name} is missing its tools"))?;
            let mut namespace = tool.clone();
            namespace
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("invalid namespace"))?
                .remove("tools");
            if children.is_empty() {
                sources.push(PolicySource::new(
                    SourceKind::ToolSchema,
                    format!("{prefix}/namespace/{name}"),
                    canonical_content(&serde_json::to_string(&namespace)?),
                ));
            }
            for child in children {
                let child_name = child
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("namespace child is missing its name"))?;
                sources.push(PolicySource::new(
                    SourceKind::ToolSchema,
                    format!("{prefix}/{name}/{child_name}"),
                    canonical_content(&serde_json::to_string(
                        &serde_json::json!({"namespace":namespace,"tool":child}),
                    )?),
                ));
            }
        } else {
            sources.push(PolicySource::new(
                SourceKind::ToolSchema,
                format!("{prefix}/functions/{name}"),
                canonical_content(&serde_json::to_string(tool)?),
            ));
        }
    }
    Ok(sources)
}

pub(crate) fn capture_deferred_tool_sources(
    tools: &[serde_json::Value],
) -> Result<Vec<PolicySource>> {
    capture_raw_tool_sources(tools, "deferred")
}

pub(crate) fn capture_code_search_sources(
    tools: &[serde_json::Value],
) -> Result<Vec<PolicySource>> {
    tools
        .iter()
        .map(|tool| {
            let name = tool
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("code-mode search result is missing its name"))?;
            let content = canonical_content(&serde_json::to_string(tool)?);
            Ok(PolicySource::new(
                SourceKind::ToolSchema,
                format!("code-mode-search/{name}"),
                content,
            ))
        })
        .collect()
}

pub(crate) fn capture_code_runtime_sources(
    tools: &[serde_json::Value],
) -> Result<Vec<PolicySource>> {
    tools
        .iter()
        .map(|tool| {
            let name = tool
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("code-mode runtime tool is missing its name"))?;
            let content = canonical_content(&serde_json::to_string(tool)?);
            Ok(PolicySource::new(
                SourceKind::ToolSchema,
                format!("code-mode-runtime/{name}"),
                content,
            ))
        })
        .collect()
}

pub fn capture_prompt_sources(prompt: &Prompt) -> Result<Vec<PolicySource>> {
    let mut sources = BTreeMap::new();
    if let Some(schema) = &prompt.output_schema {
        let content = canonical_content(&serde_json::to_string(schema)?);
        let id = format!("schema:{}", sha256(content.as_bytes()));
        sources.insert(
            (SourceKind::ToolSchema, id.clone()),
            PolicySource::new(SourceKind::ToolSchema, id, content),
        );
    }
    if !prompt.base_instructions.text.is_empty() {
        let text = prompt.base_instructions.text.clone();
        let id = format!("request-base:{}", sha256(text.as_bytes()));
        sources.insert(
            (SourceKind::Builtin, id.clone()),
            PolicySource::new(SourceKind::Builtin, id, text),
        );
    }
    for original in &prompt.input {
        if let ResponseItem::AdditionalTools { tools, .. } = original {
            for source in capture_raw_tool_sources(tools, "additional")? {
                sources.insert((source.kind.clone(), source.logical_id.clone()), source);
            }
            continue;
        }
        if let ResponseItem::ToolSearchOutput { tools, .. } = original {
            for source in capture_deferred_tool_sources(tools)? {
                sources.insert((source.kind.clone(), source.logical_id.clone()), source);
            }
            continue;
        }
        let ResponseItem::Message { role, .. } = original else {
            continue;
        };
        let developer = role == "developer" || role == "system";
        let mut item = original.clone();
        for part in to_annotated_content(&mut item).unwrap_or_default() {
            let kind = part.kind().as_str();
            if dynamic_state(kind) || (!developer && kind == "unknown") {
                continue;
            }
            if !developer && !kind.contains("instructions") && !kind.starts_with("skills.") {
                continue;
            }
            let ContentItem::InputText { text } = part.content() else {
                continue;
            };
            let text = canonical_content(text);
            let id = format!("{kind}:{}", sha256(text.as_bytes()));
            sources.insert(
                (SourceKind::InstructionFragment, id.clone()),
                PolicySource::new(SourceKind::InstructionFragment, id, text),
            );
        }
    }
    for tool in prompt.tools.iter() {
        let text = canonical_content(&serde_json::to_string(tool)?);
        let id = tool.name().to_owned();
        sources.insert(
            (SourceKind::ToolSchema, id.clone()),
            PolicySource::new(SourceKind::ToolSchema, id, text),
        );
    }
    Ok(sources.into_values().collect())
}

fn unaccepted_sources(snapshot: &PolicySnapshot, sources: Vec<PolicySource>) -> Vec<PolicySource> {
    let mut pending: Vec<_> = sources
        .into_iter()
        .filter(|source| {
            snapshot
                .verify_content(source.kind.clone(), &source.logical_id, &source.content)
                .is_err()
        })
        .filter(|source| {
            source.kind != SourceKind::InstructionFragment
                || !source.logical_id.starts_with("model_switch.instructions:")
                || !model_switch::approved_model_switch(snapshot, &source.content)
        })
        .collect();
    if pending.iter().any(|source| {
        source.kind == SourceKind::InstructionFragment
            && source.logical_id.starts_with("host_skills.instructions:")
    }) {
        let expected = codex_skills_extension::approved_host_skills_fragments(snapshot)
            .into_iter()
            .map(|text| canonical_content(&text))
            .collect::<Vec<_>>();
        pending.retain(|source| {
            source.kind != SourceKind::InstructionFragment
                || !source.logical_id.starts_with("host_skills.instructions:")
                || !expected.contains(&source.content)
        });
    }
    pending
}

pub fn verify_prompt(config: &Config, prompt: &Prompt) -> codex_protocol::error::Result<()> {
    let Some(snapshot) = &config.instruction_snapshot else {
        return Ok(());
    };
    let mut sources =
        capture_prompt_sources(prompt).map_err(|error| CodexErr::Fatal(error.to_string()))?;
    if prompt.base_instructions.text.is_empty()
        && !snapshot
            .single(SourceKind::MainPrompt)
            .map_err(|error| CodexErr::Fatal(error.to_string()))?
            .is_empty()
    {
        sources.push(PolicySource::new(
            SourceKind::Builtin,
            format!("request-base:{}", sha256(b"")),
            String::new(),
        ));
    }
    if snapshot
        .single(SourceKind::MainPrompt)
        .is_ok_and(|main| main == prompt.base_instructions.text)
    {
        sources.retain(|source| {
            source.kind != SourceKind::Builtin || !source.logical_id.starts_with("request-base:")
        });
    }
    verify_sources(config, sources)
}

pub(crate) fn verify_sources(
    config: &Config,
    sources: Vec<PolicySource>,
) -> codex_protocol::error::Result<()> {
    let Some(snapshot) = &config.instruction_snapshot else {
        return Ok(());
    };
    let pending = unaccepted_sources(snapshot, sources);
    if pending.is_empty() {
        return Ok(());
    }
    let names = pending
        .iter()
        .map(|source| format!("{:?}/{}", source.kind, source.logical_id))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sources = snapshot.sources.clone();
    for source in pending {
        sources.retain(|existing| {
            existing.kind != source.kind || existing.logical_id != source.logical_id
        });
        sources.push(source);
    }
    let candidate = PolicySnapshot::new(snapshot.config.clone(), sources)
        .map_err(|error| CodexErr::Fatal(error.to_string()))?;
    let path = config.cwd.join(format!(
        "codex-instructions-{}.pending.json",
        candidate.snapshot_id
    ));
    let write = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(&serde_json::to_vec_pretty(&candidate)?)?;
        file.sync_all()?;
        Ok(())
    })();
    let note = match write {
        Ok(()) => format!("Candidate: {}", path.display()),
        Err(error) => format!("Candidate could not be written without replacing a file: {error}"),
    };
    Err(CodexErr::Fatal(format!(
        "Instruction changes require your explicit approval: {names}. These changes were not sent to the model. {note}. Review and apply with codex instructions; restart to activate the accepted version."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_instruction_control::ForkConfig;

    fn empty_prompt() -> Prompt {
        Prompt {
            base_instructions: codex_protocol::models::BaseInstructions {
                text: String::new(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn snapshot(sources: Vec<PolicySource>) -> PolicySnapshot {
        let mut base = vec![
            PolicySource::new(SourceKind::MainPrompt, "main", "instructions".into()),
            PolicySource::new(
                SourceKind::ModelCatalog,
                "models",
                r#"{"models":[{"slug":"test","model_messages":{}}]}"#.into(),
            ),
        ];
        base.extend(sources);
        PolicySnapshot::new(ForkConfig::default(), base).unwrap()
    }

    #[test]
    fn changed_instruction_is_pending_and_does_not_modify_accepted_snapshot() {
        let accepted =
            PolicySource::new(SourceKind::InstructionFragment, "policy", "before".into());
        let original = snapshot(vec![accepted.clone()]);
        let changed = PolicySource::new(SourceKind::InstructionFragment, "policy", "after".into());
        let unknown = PolicySource::new(SourceKind::ToolSchema, "external", "new tool".into());
        assert_eq!(
            unaccepted_sources(&original, vec![accepted, changed.clone(), unknown.clone()]),
            vec![changed, unknown]
        );
        assert_eq!(
            original
                .resolve(SourceKind::InstructionFragment, "policy")
                .unwrap(),
            "before"
        );
    }

    #[test]
    fn canonicalization_only_ignores_protected_snapshot_directory_identity() {
        let digest = "a".repeat(64);
        let root = "C:/Program Files/CodexStripped/Policy/sources";
        let protected = format!("{root}/{digest}/tree/skill.md");
        assert_eq!(
            canonical_source_paths(&protected, root, &digest),
            format!("{root}/ACCEPTED_SNAPSHOT/tree/skill.md")
        );
        for text in [
            format!("hash={digest}"),
            format!("https://host/sources/{digest}/code"),
            format!("sources/{digest}/tree/skill.md"),
            format!("https://host/{protected}"),
            format!("{root}/{}/tree/skill.md", "b".repeat(64)),
        ] {
            assert_eq!(canonical_source_paths(&text, root, &digest), text);
        }
        let unicode =
            "\u{0418}\u{043d}\u{0441}\u{0442}\u{0440}\u{0443}\u{043a}\u{0446}\u{0438}\u{044f}";
        assert_eq!(canonical_source_paths(unicode, root, &digest), unicode);
    }

    #[test]
    fn model_switch_reuses_accepted_instructions_but_rejects_changed_text() {
        use crate::context::ContextualUserFragment;
        use crate::context::ModelSwitchInstructions;

        let accepted = snapshot(Vec::new());
        let rendered = ModelSwitchInstructions::new("instructions").render();
        let source = PolicySource::new(
            SourceKind::InstructionFragment,
            format!("model_switch.instructions:{}", sha256(rendered.as_bytes())),
            rendered,
        );
        assert!(unaccepted_sources(&accepted, vec![source.clone()]).is_empty());

        let changed_text = ModelSwitchInstructions::new("Changed instructions").render();
        let changed = PolicySource::new(
            SourceKind::InstructionFragment,
            format!(
                "model_switch.instructions:{}",
                sha256(changed_text.as_bytes())
            ),
            changed_text,
        );
        assert_eq!(
            unaccepted_sources(&accepted, vec![changed.clone()]),
            vec![changed]
        );
    }

    #[test]
    fn generated_fragment_labels_do_not_approve_unknown_content() {
        let accepted = snapshot(Vec::new());
        let sources = [
            "host_skills.instructions:unknown",
            "model_switch.instructions:unknown",
        ]
        .into_iter()
        .map(|name| {
            PolicySource::new(
                SourceKind::InstructionFragment,
                name,
                "New instructions".into(),
            )
        })
        .collect::<Vec<_>>();
        assert_eq!(unaccepted_sources(&accepted, sources.clone()), sources);
    }

    #[test]
    fn canonicalization_keeps_markdown_skill_paths_stable_after_snapshot_acceptance() {
        let root = r"C:\Program Files\CodexStripped\Policy\sources";
        let previous = "a".repeat(64);
        let next = "b".repeat(64);
        for prefix in [
            format!("{}/", root.replace('\\', "/")),
            format!("{root}\\"),
            format!("{}\\\\", root.replace('\\', "\\\\")),
        ] {
            let catalog = format!("- `r0` = `{prefix}{previous}/tree/skills`\n");
            let relocated = catalog.replace(&previous, &next);
            let accepted = canonical_source_paths(&catalog, root, &previous);
            assert!(accepted.contains("ACCEPTED_SNAPSHOT"));
            assert_eq!(accepted, canonical_source_paths(&relocated, root, &next));
            assert_ne!(
                accepted,
                canonical_source_paths(&format!("{relocated}Changed instructions"), root, &next)
            );
        }
    }

    #[test]
    fn deferred_namespace_approval_is_independent_of_search_result_grouping() {
        let first = serde_json::json!({"type":"namespace", "name":"calendar", "description":"Calendar tools", "tools":[
            {"type":"function", "name":"read", "description":"Read events", "parameters":{"type":"object"}}
        ]});
        let mut expanded = first.clone();
        expanded["tools"].as_array_mut().unwrap().push(serde_json::json!({
            "type":"function", "name":"create", "description":"Create event", "parameters":{"type":"object"}
        }));
        let initial = capture_deferred_tool_sources(&[first]).unwrap();
        let grouped = capture_deferred_tool_sources(&[expanded]).unwrap();
        assert_eq!(initial[0], grouped[0]);
        assert_eq!(grouped.len(), 2);
    }

    #[test]
    fn historical_tool_search_outputs_are_included_in_source_verification() {
        let tool = serde_json::json!({"type":"function", "name":"lookup", "description":"Old description", "parameters":{"type":"object"}});
        let prompt = Prompt {
            input: vec![ResponseItem::ToolSearchOutput {
                id: None,
                internal_chat_message_metadata_passthrough: None,
                call_id: Some("search-1".to_string()),
                status: "completed".to_string(),
                execution: "client".to_string(),
                tools: vec![tool],
            }],
            ..empty_prompt()
        };
        let captured = capture_prompt_sources(&prompt).unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].logical_id, "deferred/functions/lookup");
    }

    #[test]
    fn incremental_additional_tools_are_gated_even_without_top_level_specs() {
        let prompt = Prompt {
            input: vec![ResponseItem::AdditionalTools {
                id: None,
                role: "developer".to_string(),
                tools: vec![
                    serde_json::json!({"type":"namespace","name":"functions","description":"Tools","tools":[
                        {"type":"custom","name":"exec","description":"Approved execution instructions","format":{"type":"text"}}
                    ]}),
                    serde_json::json!({"type":"web_search","external_web_access":false}),
                ],
            }],
            ..empty_prompt()
        };
        let captured = capture_prompt_sources(&prompt).unwrap();
        assert_eq!(captured.len(), 2);
        assert!(
            captured
                .iter()
                .any(|source| source.logical_id == "additional/functions/exec")
        );
        assert!(
            captured
                .iter()
                .any(|source| source.logical_id == "additional/functions/web_search")
        );
    }

    #[tokio::test]
    async fn removing_nonempty_main_requires_approval_and_deliberately_empty_main_is_valid() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::config::ConfigBuilder::default()
            .codex_home(directory.path().to_path_buf())
            .harness_overrides(crate::config::ConfigOverrides {
                cwd: Some(directory.path().to_path_buf()),
                ephemeral: Some(true),
                ..Default::default()
            })
            .loader_overrides(codex_config::LoaderOverrides::without_managed_config_for_tests())
            .build()
            .await
            .unwrap();
        config.instruction_snapshot = Some(std::sync::Arc::new(snapshot(Vec::new())));
        assert!(verify_prompt(&config, &empty_prompt()).is_err());
        let mut sources = snapshot(Vec::new()).sources;
        sources
            .iter_mut()
            .find(|source| source.kind == SourceKind::MainPrompt)
            .map(|source| {
                *source = PolicySource::new(SourceKind::MainPrompt, "main", String::new())
            })
            .unwrap();
        config.instruction_snapshot = Some(std::sync::Arc::new(
            PolicySnapshot::new(ForkConfig::default(), sources).unwrap(),
        ));
        assert!(verify_prompt(&config, &empty_prompt()).is_ok());
    }

    #[tokio::test]
    async fn runtime_description_and_schema_changes_require_approval_but_no_snapshot_preserves_behavior()
     {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::config::ConfigBuilder::default()
            .codex_home(directory.path().to_path_buf())
            .harness_overrides(crate::config::ConfigOverrides {
                cwd: Some(directory.path().to_path_buf()),
                ephemeral: Some(true),
                ..Default::default()
            })
            .loader_overrides(codex_config::LoaderOverrides::without_managed_config_for_tests())
            .build()
            .await
            .unwrap();
        let definition = serde_json::json!({
            "name":"calendar_read", "description":"Read events", "input_schema":{"type":"object"}, "output_schema":null
        });
        let sources = capture_code_runtime_sources(&[definition.clone()]).unwrap();
        assert!(verify_sources(&config, sources.clone()).is_ok());
        let mut accepted = sources;
        accepted.push(PolicySource::new(
            SourceKind::MainPrompt,
            "main",
            "main prompt".to_string(),
        ));
        accepted.push(PolicySource::new(
            SourceKind::ModelCatalog,
            "models",
            r#"{"models":[{"slug":"test","model_messages":{}}]}"#.to_string(),
        ));
        let snapshot =
            std::sync::Arc::new(PolicySnapshot::new(Default::default(), accepted).unwrap());
        config.instruction_snapshot = Some(std::sync::Arc::clone(&snapshot));
        assert!(
            verify_sources(
                &config,
                capture_code_runtime_sources(&[definition.clone()]).unwrap()
            )
            .is_ok()
        );
        let mut description = definition.clone();
        description["description"] = serde_json::json!("Changed behavior instructions");
        assert!(
            verify_sources(
                &config,
                capture_code_runtime_sources(&[description]).unwrap()
            )
            .is_err()
        );
        let mut schema = definition.clone();
        schema["input_schema"] = serde_json::json!({"type":"string"});
        assert!(verify_sources(&config, capture_code_runtime_sources(&[schema]).unwrap()).is_err());
        assert_eq!(
            snapshot
                .resolve(SourceKind::ToolSchema, "code-mode-runtime/calendar_read")
                .unwrap(),
            serde_json::to_string(&definition).unwrap()
        );
    }
    #[test]
    fn output_schema_instructions_are_included_in_the_review_candidate() {
        let prompt = Prompt {
            output_schema: Some(
                serde_json::json!({"type":"object", "description":"Approved response instructions"}),
            ),
            ..empty_prompt()
        };
        let original = capture_prompt_sources(&prompt).unwrap();
        assert_eq!(original.len(), 1);
        assert!(original[0].logical_id.starts_with("schema:"));
        let mut changed = prompt;
        changed.output_schema.as_mut().unwrap()["description"] =
            serde_json::json!("Changed instructions");
        assert_ne!(original, capture_prompt_sources(&changed).unwrap());
    }
}
