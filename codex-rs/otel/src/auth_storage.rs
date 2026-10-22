//! Inert credential-storage telemetry compatibility API.
mod originator;
pub use originator::AuthStorageOriginator;
use std::error::Error;
use strum_macros::AsRefStr;

#[derive(Clone, Copy, Debug, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum CredentialKind {
    Codex,
    Mcp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum StoreMode {
    File,
    Auto,
    Keyring,
    Ephemeral,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum Store {
    File,
    DirectKeyring,
    Secrets,
    Ephemeral,
    /// A deletion spanning the selected backend, plaintext fallback, and legacy entries.
    Multiple,
}

#[derive(Clone, Copy, Debug, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum Operation {
    Load,
    Save,
    Delete,
    Cleanup,
    RefreshPersist,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum StoragePhase {
    Policy,
    Pinned,
}

#[derive(Debug, Default)]
pub struct StorageTelemetry;

impl StorageTelemetry {
    pub fn new(
        _credential_kind: CredentialKind,
        _mode: StoreMode,
        _selected_store: Store,
        _operation: Operation,
        _originator: AuthStorageOriginator,
    ) -> Self {
        Self
    }
    pub fn with_phase(self, _phase: StoragePhase) -> Self {
        self
    }
    pub fn record_secure_error(&mut self, _error: &(dyn Error + 'static)) {}
    pub fn record_load_attempt<T, E>(&mut self, _store: Store, _result: &Result<Option<T>, E>) {}
    pub fn record_save_attempt<E>(&mut self, _store: Store, _result: &Result<(), E>) {}
    pub fn record_delete_attempt<E>(&mut self, _store: Store, _result: &Result<bool, E>) {}
}

#[cfg(test)]
#[path = "auth_storage_tests.rs"]
mod tests;
