use super::ExecPolicyManager;
use codex_execpolicy::Policy;
use codex_execpolicy::RequirementsExecPolicy;
use std::sync::Arc;

impl ExecPolicyManager {
    pub(crate) fn current_for_environment(
        &self,
        environment_policy: Option<&RequirementsExecPolicy>,
    ) -> Arc<Policy> {
        let policy = self.current();
        match environment_policy {
            Some(environment_policy) => Arc::new(policy.merge_overlay(environment_policy.as_ref())),
            None => policy,
        }
    }
}

#[cfg(test)]
#[path = "model_policy_tests.rs"]
mod tests;
