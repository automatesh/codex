//! Reconstructs model context and preserves source runtime metadata across fork cutoffs.

use std::io;
use std::path::Path;

use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::ModelContextScan;
use codex_rollout::ModelContextScanProgress;
use codex_rollout::ReverseJsonlScanner;
use codex_rollout::RolloutItem;
use codex_rollout::ScanOutcome;

use super::LocalThreadStore;
use super::read_thread;
use super::rollout_lineage::RolloutLineage;
use super::thread_rollout_resolver;
use crate::LoadThreadHistoryParams;
use crate::StoredModelContext;
use crate::StoredThreadHistory;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

#[cfg(test)]
#[path = "model_context_tests.rs"]
mod tests;

#[derive(Clone, Copy)]
enum ContextExtent {
    Latest,
    Full,
}

/// Loads rollout items needed to reconstruct the latest model-visible context.
///
/// Paginated JSONL rollouts use a reverse scan. It stops at the newest `CompactedItem` with both
/// replacement history and a window number, and returns that compaction plus its newer suffix. If
/// the newest compaction lacks either field, the scan continues to the beginning of the rollout.
///
/// Compressed segments are decoded before applying their original JSONL offsets. Legacy rollouts
/// keep the existing full-history path.
pub(super) async fn load_latest_model_context(
    store: &LocalThreadStore,
    params: LoadThreadHistoryParams,
) -> ThreadStoreResult<StoredModelContext> {
    load_context(store, params, ContextExtent::Latest).await
}

pub(super) async fn load_instruction_history(
    store: &LocalThreadStore,
    params: LoadThreadHistoryParams,
) -> ThreadStoreResult<StoredThreadHistory> {
    let context = load_context(store, params, ContextExtent::Full).await?;
    Ok(StoredThreadHistory {
        thread_id: context.thread_id,
        items: context.items,
        revision: context.revision,
    })
}

pub(super) async fn load_instruction_history_for_fork(
    store: &LocalThreadStore,
    params: LoadThreadHistoryParams,
    history_base: HistoryPosition,
) -> ThreadStoreResult<StoredThreadHistory> {
    let resolved = if params.include_archived {
        thread_rollout_resolver::resolve_current_including_archived(store, params.thread_id).await?
    } else {
        thread_rollout_resolver::resolve_current(store, params.thread_id).await?
    }
    .ok_or_else(|| ThreadStoreError::InvalidRequest {
        message: format!("no rollout found for fork source {}", params.thread_id),
    })?;
    let session_meta = codex_rollout::read_session_meta_line(&resolved.path)
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: format!("failed to read fork source metadata: {error}"),
        })?;
    if session_meta.meta.id != params.thread_id
        || session_meta.meta.history_mode != ThreadHistoryMode::Paginated
    {
        return Err(ThreadStoreError::InvalidRequest {
            message: "fork source metadata does not match the paginated source thread".into(),
        });
    }
    let lineage = store
        .resolve_rollout_lineage(params.thread_id, Some(resolved.path))
        .await?
        .truncate_at(history_base)
        .await?;
    let items = scan_model_context_from_lineage(lineage, session_meta, ContextExtent::Full).await?;
    Ok(StoredThreadHistory {
        thread_id: params.thread_id,
        items,
        // A frozen prefix is not a revision of the source's current complete rollout.
        revision: None,
    })
}

async fn load_context(
    store: &LocalThreadStore,
    params: LoadThreadHistoryParams,
    extent: ContextExtent,
) -> ThreadStoreResult<StoredModelContext> {
    let resolved = if params.include_archived {
        thread_rollout_resolver::resolve_current_including_archived(store, params.thread_id).await?
    } else {
        thread_rollout_resolver::resolve_current(store, params.thread_id).await?
    };
    let path =
        resolved
            .map(|resolved| resolved.path)
            .ok_or_else(|| ThreadStoreError::InvalidRequest {
                message: format!("no rollout found for thread id {}", params.thread_id),
            })?;

    load_from_rollout_path_with_extent(store, params.thread_id, &path, extent).await
}

pub(super) async fn load_from_rollout_path(
    store: &LocalThreadStore,
    thread_id: codex_protocol::ThreadId,
    path: &Path,
) -> ThreadStoreResult<StoredModelContext> {
    load_from_rollout_path_with_extent(store, thread_id, path, ContextExtent::Latest).await
}

async fn load_from_rollout_path_with_extent(
    store: &LocalThreadStore,
    thread_id: codex_protocol::ThreadId,
    path: &Path,
    extent: ContextExtent,
) -> ThreadStoreResult<StoredModelContext> {
    let before = super::history_revision::read(path).await;
    let session_meta = codex_rollout::read_session_meta_line(path)
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to read session metadata {}: {err}", path.display()),
        })?;
    if session_meta.meta.id != thread_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "rollout at {} belongs to thread {}, not {}",
                path.display(),
                session_meta.meta.id,
                thread_id
            ),
        });
    }

    let items = if matches!(session_meta.meta.history_mode, ThreadHistoryMode::Paginated) {
        let lineage = store
            .resolve_rollout_lineage(thread_id, Some(path.to_path_buf()))
            .await?;
        scan_model_context_from_lineage(lineage, session_meta, extent).await?
    } else {
        read_thread::load_history_items(path).await?
    };

    let after = super::history_revision::read(path).await;
    Ok(StoredModelContext {
        revision: before.filter(|revision| Some(revision) == after.as_ref()),
        thread_id,
        items,
    })
}

/// Loads startup context from a fork's frozen inherited prefix.
pub(super) async fn load_for_fork(
    lineage: RolloutLineage,
    history_base: Option<HistoryPosition>,
) -> ThreadStoreResult<Vec<RolloutItem>> {
    let source_path = lineage
        .segments()
        .last()
        .map(|segment| segment.rollout_path.as_path())
        .ok_or_else(|| ThreadStoreError::Internal {
            message: "fork lineage has no source segment".to_string(),
        })?;
    let mut session_meta = codex_rollout::read_session_meta_line(source_path)
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!(
                "failed to read session metadata {}: {err}",
                source_path.display()
            ),
        })?;
    if session_meta.meta.multi_agent_version.is_none() {
        // Recover only the runtime version before applying the fork cutoff. Stop at the
        // newest version-bearing context instead of retaining the source's full replay.
        let source_lineage = lineage.clone();
        session_meta.meta.multi_agent_version = tokio::task::spawn_blocking(move || {
            for segment in source_lineage.segments().iter().rev() {
                let file =
                    codex_rollout::open_rollout_seekable_reader(segment.rollout_path.as_path())?;
                let mut scanner = match segment.end.map(|end| end.end_byte_offset) {
                    Some(end_byte_offset) => ReverseJsonlScanner::new_at(file, end_byte_offset)?,
                    None => ReverseJsonlScanner::new(file)?,
                };
                while let Some(outcome) = scanner.scan_next_rollout_line()? {
                    let ScanOutcome::Parsed(line) = outcome else {
                        continue;
                    };
                    if let Some(version) = codex_rollout::resume_multi_agent_version(&line.item) {
                        return Ok(Some(version));
                    }
                    // Ancestor metadata does not describe the immediate source's runtime.
                    if matches!(line.item, RolloutItem::SessionMeta(_)) {
                        break;
                    }
                }
            }
            Ok::<_, io::Error>(None)
        })
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to join fork runtime version scan: {err}"),
        })?
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to read fork runtime version: {err}"),
        })?;
    }
    match history_base {
        Some(history_base) => {
            let lineage = lineage.truncate_at(history_base).await?;
            scan_model_context_from_lineage(lineage, session_meta, ContextExtent::Latest).await
        }
        None => Ok(vec![RolloutItem::SessionMeta(session_meta)]),
    }
}

async fn scan_model_context_from_lineage(
    lineage: RolloutLineage,
    session_meta: SessionMetaLine,
    extent: ContextExtent,
) -> ThreadStoreResult<Vec<RolloutItem>> {
    let scan = tokio::task::spawn_blocking(move || {
        scan_model_context_from_lineage_blocking(&lineage, session_meta, extent)
    })
    .await
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to join model context scan: {err}"),
    })?;
    match scan {
        Ok(items) => Ok(items),
        Err(err) => Err(ThreadStoreError::Internal {
            message: format!("failed to scan paginated model context lineage: {err}"),
        }),
    }
}

fn scan_model_context_from_lineage_blocking(
    lineage: &RolloutLineage,
    session_meta: SessionMetaLine,
    extent: ContextExtent,
) -> io::Result<Vec<RolloutItem>> {
    let mut scan = ModelContextScan::default();
    let mut full_history = Vec::new();
    'segments: for segment in lineage.segments().iter().rev() {
        let file = codex_rollout::open_rollout_seekable_reader(segment.rollout_path.as_path())?;
        let mut scanner = match segment.end.map(|end| end.end_byte_offset) {
            Some(end_byte_offset) => ReverseJsonlScanner::new_at(file, end_byte_offset)?,
            None => ReverseJsonlScanner::new(file)?,
        };
        while let Some(outcome) = scanner.scan_next_rollout_line()? {
            let line = match outcome {
                ScanOutcome::Parsed(line) => line,
                ScanOutcome::Rejected(error) if matches!(extent, ContextExtent::Full) => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, error));
                }
                ScanOutcome::Rejected(_) => continue,
            };
            // Each rollout segment contributes only its local delta. Its session metadata is
            // replaced with the requested thread's canonical SessionMeta after replay.
            if matches!(&line.item, RolloutItem::SessionMeta(_)) {
                break;
            }
            match extent {
                ContextExtent::Full => full_history.push(line.item),
                ContextExtent::Latest => match scan.push(line.item) {
                    ModelContextScanProgress::Continue => {}
                    ModelContextScanProgress::Complete => break 'segments,
                },
            }
        }
    }

    let mut items = match extent {
        ContextExtent::Latest => scan.finish(),
        ContextExtent::Full => {
            full_history.reverse();
            full_history
        }
    };
    items.insert(0, RolloutItem::SessionMeta(session_meta));
    Ok(items)
}
