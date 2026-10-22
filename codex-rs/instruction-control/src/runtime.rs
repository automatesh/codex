use crate::PolicySnapshot;
use anyhow::Result;
use anyhow::bail;
use std::sync::Arc;
use std::sync::OnceLock;

static SNAPSHOT: OnceLock<Arc<PolicySnapshot>> = OnceLock::new();

pub fn initialize_runtime() -> Result<Arc<PolicySnapshot>> {
    if let Some(snapshot) = SNAPSHOT.get() {
        return Ok(Arc::clone(snapshot));
    }
    let snapshot = Arc::new(PolicySnapshot::load_protected()?);
    if SNAPSHOT.set(Arc::clone(&snapshot)).is_err() {
        let current = SNAPSHOT.get().expect("snapshot initialized concurrently");
        if current.snapshot_id != snapshot.snapshot_id {
            bail!("accepted instructions changed during startup; restart Codex");
        }
        return Ok(Arc::clone(current));
    }
    Ok(snapshot)
}

pub fn runtime_snapshot() -> Option<Arc<PolicySnapshot>> {
    SNAPSHOT.get().map(Arc::clone)
}
