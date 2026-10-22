mod world_state;

use std::sync::Arc;

use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;

struct GitAttributionExtension {
    commit_trailer: String,
    pr_footer: String,
}

impl ContextContributor for GitAttributionExtension {
    fn contribute_world_state<'a>(
        &'a self,
        _input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        Box::pin(async move {
            vec![world_state::git_attribution_world_state_section(
                self.commit_trailer.clone(),
                self.pr_footer.clone(),
            )]
        })
    }
}

/// Installs attribution supplied exclusively by the local user configuration.
pub fn install<C: Sync>(
    registry: &mut ExtensionRegistryBuilder<C>,
    commit_trailer: String,
    pr_footer: String,
) {
    registry.prompt_contributor(Arc::new(GitAttributionExtension {
        commit_trailer,
        pr_footer,
    }));
}

#[cfg(test)]
#[path = "git_attribution_tests.rs"]
mod tests;
