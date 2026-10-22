use codex_extension_api::PreviousWorldStateSection;
use serde_json::json;

use super::world_state::git_attribution_world_state_section;

#[test]
fn empty_local_settings_do_not_inject_attribution() {
    let section = git_attribution_world_state_section(String::new(), String::new());
    let (snapshot, fragment) = section.render_diff(PreviousWorldStateSection::Absent);
    assert_eq!(
        snapshot,
        Some(json!({"commit_trailer": "", "pr_footer": ""}))
    );
    assert!(fragment.is_none());
}

#[test]
fn resumed_backend_policy_is_replaced_with_empty_local_settings() {
    let section = git_attribution_world_state_section(String::new(), String::new());
    let (_, fragment) = section.render_diff(PreviousWorldStateSection::Known(&json!(true)));
    let body = fragment
        .expect("old enabled attribution needs an update")
        .body()
        .to_string();
    assert!(body.contains("Stop applying earlier generated attribution instructions"));
    assert!(!body.contains("noreply@openai.com"));
    assert!(!body.contains("Generated with"));
}

#[test]
fn configured_values_are_independent_and_emitted_once_per_change() {
    let section = git_attribution_world_state_section(
        "Co-authored-by: Example <example@example.test>".to_string(),
        String::new(),
    );
    let (snapshot, fragment) = section.render_diff(PreviousWorldStateSection::Absent);
    let fragment = fragment.expect("configured trailer should be emitted");
    assert!(fragment.body().contains("example@example.test"));
    assert!(!fragment.body().contains("footer exactly once"));
    let (_, repeated) =
        section.render_diff(PreviousWorldStateSection::Known(snapshot.as_ref().unwrap()));
    assert!(repeated.is_none());
    let wrapped = format!("<git_attribution>\n{}\n</git_attribution>", fragment.body());
    assert!(section.matches_retained_fragment("developer", &wrapped));
    assert!(!section.matches_retained_fragment("user", &wrapped));

    let updated = git_attribution_world_state_section(String::new(), "Made locally".to_string());
    let (_, fragment) =
        updated.render_diff(PreviousWorldStateSection::Known(snapshot.as_ref().unwrap()));
    let fragment = fragment.expect("changed settings should be emitted");
    assert!(fragment.body().contains("Made locally"));
    assert!(!fragment.body().contains("example@example.test"));
}

#[test]
fn unknown_resumed_state_receives_explicit_local_policy() {
    let section = git_attribution_world_state_section(String::new(), String::new());
    let (_, fragment) = section.render_diff(PreviousWorldStateSection::Unknown);
    assert!(fragment.is_some());
}
