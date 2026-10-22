use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::ToolStartInput;
use tokio::sync::Notify;

#[derive(Default)]
pub(super) struct ForkTestLifecycle {
    pub(super) entered: Notify,
    release: Notify,
}

impl ToolLifecycleContributor for ForkTestLifecycle {
    fn on_tool_start<'a>(&'a self, input: ToolStartInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if input.call_id == "child-pause" {
                self.entered.notify_one();
                self.release.notified().await;
            }
        })
    }
}
