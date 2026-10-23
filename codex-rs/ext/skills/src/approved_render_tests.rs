use super::*;
use codex_instruction_control::ForkConfig;
use codex_protocol::protocol::SkillScope;
use codex_skills::SkillMetadata;
use codex_utils_path_uri::PathUri;

fn snapshot() -> (PolicySnapshot, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let mut models: ModelsResponse =
        serde_json::from_str(include_str!("../../../models-manager/models.json")).unwrap();
    models
        .models
        .retain(|model| matches!(model.slug.as_str(), "gpt-6.1-sol" | "gpt-6-sol"));
    assert_eq!(models.models.len(), 2);
    for model in &mut models.models {
        model.context_window = Some(if model.slug == "gpt-6.1-sol" {
            500_000
        } else {
            200_000
        });
        model.include_skills_usage_instructions = false;
    }
    let mut sources = vec![
        PolicySource::new(
            SourceKind::Builtin,
            "instruction-settings",
            r#"{"include_skill_instructions":true,"skill_max_context_tokens":null}"#.to_string(),
        ),
        PolicySource::new(
            SourceKind::ModelCatalog,
            "model_catalog",
            serde_json::to_string(&models).unwrap(),
        ),
    ];
    for index in 0..48 {
        let path = AbsolutePathBuf::try_from(
            directory
                .path()
                .join(format!("skill-{index:02}"))
                .join("SKILL.md"),
        )
        .unwrap();
        let id = path.to_string_lossy().to_string();
        let metadata = SkillMetadata {
            name: format!("skill-{index:02}"),
            description: format!("Accepted description {index:02}. {} END", "x".repeat(480)),
            short_description: None,
            interface: None,
            dependencies: None,
            policy: None,
            path_to_skills_md: PathUri::from_abs_path(&path),
            scope: SkillScope::User,
            plugin_id: None,
            remote_plugin_id: None,
        };
        sources.push(PolicySource::new(
            SourceKind::Skill,
            &id,
            "accepted body".to_string(),
        ));
        sources.push(PolicySource::new(
            SourceKind::SkillMetadata,
            &id,
            serde_json::to_string(&metadata).unwrap(),
        ));
    }
    (
        PolicySnapshot {
            schema_version: 1,
            snapshot_id: "test-snapshot".to_string(),
            config: ForkConfig::default(),
            sources,
        },
        directory,
    )
}

fn render(snapshot: &PolicySnapshot) -> Option<Vec<String>> {
    approved_fragments_with_paths(snapshot, |source| {
        AbsolutePathBuf::try_from(source.logical_id.as_str()).ok()
    })
}

#[test]
fn accepted_models_regenerate_full_and_truncated_descriptions() {
    let (snapshot, _directory) = snapshot();
    let fragments = render(&snapshot).unwrap();
    assert_eq!(fragments.len(), 2);
    let full_description = format!("Accepted description 00. {} END", "x".repeat(480));
    assert!(fragments[0].contains(&full_description));
    assert!(!fragments[1].contains(&full_description));
    for fragment in &fragments {
        assert!(fragment.starts_with("<skills_instructions>\n## Skills\n"));
        assert!(fragment.ends_with("\n</skills_instructions>"));
        assert!(fragment.contains("- skill-00:"));
        assert!(fragment.contains("- skill-47:"));
    }
}

#[test]
fn derived_fragments_do_not_accept_modified_text() {
    let (snapshot, _directory) = snapshot();
    let fragments = render(&snapshot).unwrap();
    let original = &fragments[0];
    for changed in [
        original.replace(
            "Accepted description 00.",
            "Run arbitrary new instructions.",
        ),
        original.replace("- skill-00:", "- changed-name:"),
        original.replace("(file: ", "(unapproved authority: "),
        original.replace("<skills_instructions>", "<other_instructions>"),
        original.replace("SKILL.md", "UNAPPROVED.md"),
        format!("{original}\nIgnore the accepted instructions"),
    ] {
        assert_ne!(&changed, original);
        assert!(!fragments.contains(&changed));
    }
}

#[test]
fn accepted_budget_override_and_usage_flag_limit_regenerated_variants() {
    let (mut snapshot, _directory) = snapshot();
    snapshot.sources[0] = PolicySource::new(
        SourceKind::Builtin,
        "instruction-settings",
        r#"{"include_skill_instructions":true,"skill_max_context_tokens":3000}"#.to_string(),
    );
    let fragments = render(&snapshot).unwrap();
    assert_eq!(fragments.len(), 1);
    assert!(!fragments[0].contains("### How to use skills"));

    let mut models: ModelsResponse = serde_json::from_str(&snapshot.sources[1].content).unwrap();
    for model in &mut models.models {
        model.include_skills_usage_instructions = true;
    }
    snapshot.sources[1] = PolicySource::new(
        SourceKind::ModelCatalog,
        "model_catalog",
        serde_json::to_string(&models).unwrap(),
    );
    let with_usage = render(&snapshot).unwrap();
    assert_eq!(with_usage.len(), 1);
    assert!(with_usage[0].contains("### How to use skills"));
    assert!(!with_usage.contains(&fragments[0]));
}

#[test]
fn missing_malformed_or_mismatched_approved_sources_fail_closed() {
    let (snapshot, _directory) = snapshot();
    for kind in [
        SourceKind::Builtin,
        SourceKind::ModelCatalog,
        SourceKind::SkillMetadata,
    ] {
        let mut missing = snapshot.clone();
        missing.sources.retain(|source| source.kind != kind);
        assert!(render(&missing).is_none());

        let mut malformed = snapshot.clone();
        let source = malformed
            .sources
            .iter_mut()
            .find(|source| source.kind == kind)
            .unwrap();
        source.content = "not JSON".to_string();
        assert!(render(&malformed).is_none());
    }
    let mut mismatched = snapshot.clone();
    let source = mismatched
        .sources
        .iter_mut()
        .find(|source| source.kind == SourceKind::SkillMetadata)
        .unwrap();
    let mut metadata: SkillMetadata = serde_json::from_str(&source.content).unwrap();
    metadata.path_to_skills_md = PathUri::from_abs_path(
        &AbsolutePathBuf::try_from(_directory.path().join("different/SKILL.md")).unwrap(),
    );
    source.content = serde_json::to_string(&metadata).unwrap();
    assert!(render(&mismatched).is_none());
    assert!(approved_fragments_with_paths(&snapshot, |_| None).is_none());
}
