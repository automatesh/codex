use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;

use codex_extension_api::Instructions;
use codex_extension_api::LoadInstructionsFuture;
use codex_extension_api::LoadedUserInstructions;
use codex_extension_api::UserInstructionsProvider;
use codex_instruction_control::PolicySnapshot;
use codex_instruction_control::SourceKind;
use codex_utils_absolute_path::AbsolutePathBuf;

const DEFAULT_AGENTS_MD_FILENAME: &str = "AGENTS.md";
const LOCAL_AGENTS_MD_FILENAME: &str = "AGENTS.override.md";

/// Loads user instructions from a Codex home directory.
#[derive(Clone, Debug)]
pub struct CodexHomeUserInstructionsProvider {
    codex_home: AbsolutePathBuf,
    instruction_snapshot: Option<Arc<PolicySnapshot>>,
    // Cached instructions and warning history are shared across provider clones.
    state: Arc<Mutex<InstructionsState>>,
}

#[derive(Debug, Default)]
struct InstructionsState {
    // Failed refreshes retain the last good value; confirmed absence still clears it.
    last_successful_instructions: Option<Instructions>,
    // Report ongoing failures once, then again if they return after recovery.
    active_warnings: HashSet<String>,
}

impl CodexHomeUserInstructionsProvider {
    /// Creates a provider rooted at the supplied absolute Codex home directory.
    pub fn new(codex_home: AbsolutePathBuf) -> Self {
        Self {
            codex_home,
            instruction_snapshot: None,
            state: Arc::new(Mutex::new(InstructionsState::default())),
        }
    }

    /// Pins global instruction text to a reviewed snapshot for the provider lifetime.
    pub fn with_instruction_snapshot(mut self, snapshot: Option<Arc<PolicySnapshot>>) -> Self {
        self.instruction_snapshot = snapshot;
        self.state = Arc::new(Mutex::new(InstructionsState::default()));
        self
    }

    async fn load_approved_instructions(
        &self,
        snapshot: &PolicySnapshot,
    ) -> LoadedUserInstructions {
        let mut warnings = Vec::new();
        let home = snapshot
            .resolve(SourceKind::Builtin, "global-agents-home")
            .map_err(|error| error.to_string())
            .and_then(|path| {
                AbsolutePathBuf::try_from(std::path::PathBuf::from(path))
                    .map_err(|error| error.to_string())
            });
        let home = match home {
            Ok(home) => home,
            Err(error) => {
                return LoadedUserInstructions {
                    instructions: None,
                    warnings: vec![format!(
                        "Accepted global instruction home is missing or invalid: {error}; capture and approve a complete candidate"
                    )],
                };
            }
        };
        for candidate in [LOCAL_AGENTS_MD_FILENAME, DEFAULT_AGENTS_MD_FILENAME] {
            let path = home.join(candidate);
            let id = path.to_string_lossy().to_string();
            let current = tokio::fs::read(path.as_path()).await;
            match snapshot.resolve(SourceKind::Agents, &id) {
                Ok(accepted) => {
                    if !current
                        .as_ref()
                        .is_ok_and(|bytes| bytes == accepted.as_bytes())
                    {
                        warnings.push(format!(
                            "Global instructions `{id}` changed or are unavailable; using the accepted snapshot. Prepare and approve a candidate to update them."
                        ));
                    }
                    if !accepted.trim().is_empty() {
                        return LoadedUserInstructions {
                            instructions: Some(Instructions {
                                text: accepted.trim().to_string(),
                                source: Some(path),
                            }),
                            warnings,
                        };
                    }
                }
                Err(_) => {
                    if current.as_ref().is_ok_and(|bytes| !bytes.is_empty()) {
                        warnings.push(format!(
                            "Global instructions `{id}` are not in the accepted snapshot and were not loaded. Prepare and approve a candidate to include them."
                        ));
                    }
                }
            }
        }
        LoadedUserInstructions {
            instructions: None,
            warnings,
        }
    }

    #[tracing::instrument(name = "instructions.load", skip_all, fields(provider = "global"))]
    async fn load_from_codex_home(&self) -> LoadedUserInstructions {
        if let Some(snapshot) = &self.instruction_snapshot {
            return self.load_approved_instructions(snapshot).await;
        }
        let mut warnings = Vec::new();
        for candidate in [LOCAL_AGENTS_MD_FILENAME, DEFAULT_AGENTS_MD_FILENAME] {
            let path = self.codex_home.join(candidate);
            match tokio::fs::metadata(path.as_path()).await {
                Ok(metadata) if !metadata.is_file() => continue,
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    warnings.push(format!(
                        "Failed to read global AGENTS.md instructions from `{}`: {err}",
                        path.display()
                    ));
                    continue;
                }
            }
            let data = match tokio::fs::read(path.as_path()).await {
                Ok(data) => data,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    warnings.push(format!(
                        "Failed to read global AGENTS.md instructions from `{}`: {err}",
                        path.display()
                    ));
                    continue;
                }
            };
            let contents = String::from_utf8_lossy(&data);
            let trimmed = contents.trim();
            if !trimmed.is_empty() {
                return LoadedUserInstructions {
                    instructions: Some(Instructions {
                        text: trimmed.to_string(),
                        source: Some(path),
                    }),
                    warnings,
                };
            }
        }
        LoadedUserInstructions {
            instructions: None,
            warnings,
        }
    }
}

impl UserInstructionsProvider for CodexHomeUserInstructionsProvider {
    fn load_user_instructions(&self) -> LoadInstructionsFuture<'_> {
        Box::pin(async move {
            let mut loaded = self.load_from_codex_home().await;
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if loaded.instructions.is_some() || loaded.warnings.is_empty() {
                state
                    .last_successful_instructions
                    .clone_from(&loaded.instructions);
            } else {
                loaded.instructions = state.last_successful_instructions.clone();
            }
            let previous_warnings = std::mem::take(&mut state.active_warnings);
            loaded.warnings.retain(|warning| {
                state.active_warnings.insert(warning.clone())
                    && !previous_warnings.contains(warning)
            });
            loaded
        })
    }
}

#[cfg(test)]
mod tests;
