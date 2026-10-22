//! Compatibility types for skill-use call sites; this fork does not record invocations.

use crate::SessionTelemetry;
use codex_protocol::protocol::SkillScope;

/// How Codex detected the skill's use.
pub enum SkillInvocationType {
    Explicit,
    Implicit,
}

/// Metadata for a detected skill invocation. Never include skill contents or resource paths.
pub struct SkillInvocationEvent<'a> {
    pub turn_id: &'a str,
    pub skill_name: &'a str,
    pub scope: Option<SkillScope>,
    pub plugin_id: Option<&'a str>,
    pub invocation_type: SkillInvocationType,
}

impl SessionTelemetry {
    pub fn with_user_id(self, _user_id: Option<String>) -> Self {
        self
    }

    pub fn skill_invocation(&self, _event: SkillInvocationEvent<'_>) {}
}
