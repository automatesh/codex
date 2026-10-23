use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroUsize;

use codex_extension_api::ContextualUserFragment;
use codex_instruction_control::PolicySnapshot;
use codex_instruction_control::PolicySource;
use codex_instruction_control::SourceKind;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_OPEN_TAG;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;

use crate::SkillLoadOutcome;
use crate::approved_skills::approved_skill_metadata;
use crate::provider::catalog_from_outcome;
use crate::render::SkillCatalogRenderPolicy;
use crate::render::render_available_skills;
use crate::render::skill_metadata_budget;

#[derive(Deserialize)]
struct ApprovedSettings {
    include_skill_instructions: bool,
    skill_max_context_tokens: Option<NonZeroUsize>,
}

/// Exact host-only catalog renderings derived from accepted sources and model budgets.
/// Combined executor/cloud catalogs require their own approval and are not inferred here.
pub fn approved_host_skills_fragments(snapshot: &PolicySnapshot) -> Vec<String> {
    approved_fragments_with_paths(snapshot, |source| {
        AbsolutePathBuf::try_from(
            snapshot
                .source_path(SourceKind::Skill, &source.logical_id)
                .ok()?,
        )
        .ok()
    })
    .unwrap_or_default()
}

fn approved_fragments_with_paths(
    snapshot: &PolicySnapshot,
    path_for_source: impl Fn(&PolicySource) -> Option<AbsolutePathBuf>,
) -> Option<Vec<String>> {
    let settings: ApprovedSettings = serde_json::from_str(
        snapshot
            .resolve(SourceKind::Builtin, "instruction-settings")
            .ok()?,
    )
    .ok()?;
    if !settings.include_skill_instructions {
        return Some(Vec::new());
    }
    let models: ModelsResponse =
        serde_json::from_str(snapshot.single(SourceKind::ModelCatalog).ok()?).ok()?;
    if models.models.is_empty() {
        return None;
    }

    let mut skills = Vec::new();
    let mut roots = Vec::new();
    let mut root_by_path = HashMap::new();
    for source in snapshot
        .sources
        .iter()
        .filter(|source| source.kind == SourceKind::Skill)
    {
        let path = path_for_source(source)?;
        let skill = approved_skill_metadata(snapshot, source, &path).ok()?;
        let root = path.parent()?.to_owned();
        roots.push(root.clone());
        root_by_path.insert(skill.path_to_skills_md.clone(), root);
        skills.push(skill);
    }
    let outcome = SkillLoadOutcome::from_parts(
        skills,
        Vec::new(),
        roots,
        root_by_path,
        HashMap::new(),
        HashSet::new(),
        HashMap::new(),
    );
    let catalog = catalog_from_outcome(&outcome);
    let mut profiles = Vec::new();
    let mut fragments = Vec::new();
    for model in models.models {
        let profile = (
            skill_metadata_budget(
                model.resolved_context_window(),
                settings.skill_max_context_tokens,
            ),
            model.include_skills_usage_instructions,
        );
        if profiles.contains(&profile) {
            continue;
        }
        profiles.push(profile);
        if let Some(fragment) = render_available_skills(
            &catalog,
            SkillCatalogRenderPolicy::CoreCompatible,
            profile.0,
            profile.1,
        )
        .and_then(|rendered| rendered.into_fragment(profile.1))
        {
            let body = fragment.body();
            let text =
                format!("{SKILLS_INSTRUCTIONS_OPEN_TAG}{body}{SKILLS_INSTRUCTIONS_CLOSE_TAG}");
            if !fragments.contains(&text) {
                fragments.push(text);
            }
        }
    }
    Some(fragments)
}

#[cfg(test)]
#[path = "approved_render_tests.rs"]
mod tests;
