use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use codex_config::ConfigLayerStack;
use codex_instruction_control::PolicySnapshot;
use codex_instruction_control::SourceKind;
use codex_skills::SkillError;
use codex_skills::SkillMetadata;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;

use crate::HostSkillsSnapshot;
use crate::SkillLoadOutcome;

pub(crate) async fn load_approved_skills(
    snapshot: &PolicySnapshot,
    cwd: &AbsolutePathBuf,
    _config: &ConfigLayerStack,
) -> HostSkillsSnapshot {
    let mut skills = Vec::new();
    let mut errors = Vec::new();
    let mut roots = Vec::new();
    let mut root_by_path = HashMap::new();
    for source in snapshot
        .sources
        .iter()
        .filter(|source| source.kind == SourceKind::Skill)
    {
        let loaded = async {
            let path = AbsolutePathBuf::try_from(
                snapshot.source_path(SourceKind::Skill, &source.logical_id)?,
            )?;
            let metadata = snapshot.resolve(SourceKind::SkillMetadata, &source.logical_id)?;
            let mut skill: SkillMetadata = serde_json::from_str(metadata)?;
            let original_path = skill.path_to_skills_md.to_abs_path()?;
            if original_path.to_string_lossy() != source.logical_id {
                return Err::<_, Box<dyn std::error::Error + Send + Sync>>(
                    "Approved skill metadata belongs to a different source path".into(),
                );
            }
            skill.path_to_skills_md = PathUri::from_abs_path(&path);
            if let Some(interface) = &mut skill.interface {
                for icon in [&mut interface.icon_small, &mut interface.icon_large] {
                    if let Some(original) = icon {
                        *original = AbsolutePathBuf::try_from(
                            snapshot
                                .source_path(SourceKind::SkillAsset, &original.to_string_lossy())?,
                        )?;
                    }
                }
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>((path, skill))
        }
        .await;
        match loaded {
            Ok((path, skill)) => {
                if let Some(root) = path.parent() {
                    let root = root.to_owned();
                    roots.push(root.clone());
                    root_by_path.insert(skill.path_to_skills_md.clone(), root);
                }
                if let Ok(current) = std::fs::read(&source.logical_id)
                    && current != source.content.as_bytes()
                {
                    errors.push(SkillError {
                        path: path.clone(),
                        message: format!("Source {} changed; the accepted skill package remains active. Prepare and approve a candidate to update it.", source.logical_id),
                    });
                }
                skills.push(skill);
            }
            Err(error) => errors.push(SkillError {
                path: cwd.clone(),
                message: format!("Accepted skill {} is unavailable: {error}. No live source fallback was loaded.", source.logical_id),
            }),
        }
    }
    if skills.is_empty() {
        errors.push(SkillError {
            path: cwd.clone(),
            message: "Only skills in the accepted instruction snapshot are available. New skills and package resources require candidate approval.".to_string(),
        });
    }
    let outcome = SkillLoadOutcome::from_parts(
        skills,
        errors,
        roots,
        root_by_path,
        HashMap::new(),
        HashSet::new(),
        HashMap::new(),
    )
    .with_disabled_paths(HashSet::new());
    HostSkillsSnapshot::new(Arc::new(outcome))
}
