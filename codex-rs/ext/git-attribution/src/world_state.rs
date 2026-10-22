use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateSectionContribution;

const WORLD_STATE_ID: &str = "git_attribution";
const START_MARKER: &str = "<git_attribution>";
const END_MARKER: &str = "</git_attribution>";
pub(super) const LEGACY_COMMIT_ATTRIBUTION_INSTRUCTIONS: &str = "\
When you write or edit a git commit message, ensure the message ends with this trailer exactly once:
Co-authored-by: Codex <noreply@openai.com>

Rules:
- Keep existing trailers and append this trailer at the end if missing.
- Do not duplicate this trailer if it already exists.
- Keep one blank line between the commit body and trailer block.";
const DISABLED_INSTRUCTIONS: &str = "Local commit and pull request attribution is empty. Stop applying earlier generated attribution instructions; preserve existing messages unless otherwise requested by the user.";

pub(super) fn git_attribution_world_state_section(
    commit_trailer: String,
    pr_footer: String,
) -> WorldStateSectionContribution {
    let commit_trailer = commit_trailer.trim().to_string();
    let pr_footer = pr_footer.trim().to_string();
    let enabled = !commit_trailer.is_empty() || !pr_footer.is_empty();
    let snapshot = serde_json::json!({
        "commit_trailer": commit_trailer,
        "pr_footer": pr_footer,
    });
    let mut instructions = String::new();
    if !commit_trailer.is_empty() {
        instructions.push_str("When writing or editing a commit message, include this locally configured trailer exactly once, preserving existing trailers:\n");
        instructions.push_str(&commit_trailer);
        instructions.push('\n');
    }
    if !pr_footer.is_empty() {
        instructions.push_str("When creating or editing a pull request body, include this locally configured footer exactly once, preserving existing content:\n");
        instructions.push_str(&pr_footer);
        instructions.push('\n');
    }
    if enabled {
        instructions.push_str("Do not rewrite an existing commit or pull request solely to add attribution. These local settings replace earlier generated attribution instructions.");
    }
    let retained_instructions = instructions.clone();
    let contribution = WorldStateSectionContribution::new(WORLD_STATE_ID, move |previous| {
        let unchanged =
            matches!(previous, PreviousWorldStateSection::Known(value) if value == &snapshot);
        let disabled_from_start = !enabled && matches!(previous, PreviousWorldStateSection::Absent);
        let fragment = if unchanged || disabled_from_start {
            None
        } else {
            Some(RenderedWorldStateFragment::new(
                "developer",
                (START_MARKER, END_MARKER),
                if enabled {
                    instructions.clone()
                } else {
                    DISABLED_INSTRUCTIONS.to_string()
                },
            ))
        };
        (Some(snapshot.clone()), fragment)
    })
    .with_legacy_matcher(|role, text| {
        is_enabled_fragment(role, text) || is_legacy_commit_attribution_fragment(role, text)
    });
    if enabled {
        contribution.with_retained_fragment_matcher(move |role, text| {
            role == "developer"
                && text.trim() == format!("{START_MARKER}\n{retained_instructions}\n{END_MARKER}")
        })
    } else {
        contribution
    }
}

fn is_legacy_commit_attribution_fragment(role: &str, text: &str) -> bool {
    role == "developer" && text.trim() == LEGACY_COMMIT_ATTRIBUTION_INSTRUCTIONS
}

fn is_enabled_fragment(role: &str, text: &str) -> bool {
    role == "developer"
        && text.trim_start().starts_with(START_MARKER)
        && text.contains("Co-authored-by: Codex <noreply@openai.com>")
        && (text.contains("Generated with [Codex](https://openai.com/codex/).")
            || text.contains("Generated with Codex."))
        && text.trim_end().ends_with(END_MARKER)
}
