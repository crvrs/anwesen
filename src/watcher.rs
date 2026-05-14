//! Filesystem-event mapping + debouncer for [ANW-16].
//!
//! The [`FilesystemWatcher`](crate::app::FilesystemWatcher) `GenServer` owns a
//! [`notify::RecommendedWatcher`] over the vault root. Each native event is
//! pushed into a Tokio channel; [`run_debouncer`] drains the channel,
//! classifies events into [`WatchAction`]s, coalesces a 100 ms window's
//! worth into one [`Batch`], and casts the batch to the `index_writer`
//! named process for a single [`crate::store::NoteStore`] write.
//!
//! See [[ADR-003 Filesystem Change Tracking]] for the event model.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hydra::{Dest, GenServer};
use notify::Event;
use notify::EventKind;
use notify::event::{AccessKind, AccessMode, Flag, ModifyKind, RenameMode};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::app::{INDEX_WRITER_NAME, IndexBatch, IndexWriterMessage, IndexWriterState};
use crate::app::{VAULT_SCANNER_NAME, VaultScanner, VaultScannerMessage};
use crate::health::HealthState;
use crate::vault;

/// One path-scoped action derived from a native filesystem event. Always
/// carries a vault-relative, forward-slash-normalized path string -- the
/// same form [`vault::Note.path`] uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchAction {
    Upsert(String),
    Delete(String),
}

/// One debouncer window's worth of coalesced changes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WatchBatch {
    pub upserts: Vec<String>,
    pub deletes: Vec<String>,
}

/// Classify one [`Event`] into zero or more [`WatchAction`]s. Dot-segments
/// anywhere in the path and non-`.md` files are dropped here so the
/// downstream batch is already filtered.
#[must_use]
pub fn map_event(event: &Event, vault_root: &Path) -> Vec<WatchAction> {
    if is_overflow(event) {
        // Overflow is a vault-wide signal, not a per-path action -- the
        // caller dispatches `rescan_now` to `vault_scanner` instead.
        return Vec::new();
    }
    let mut actions = Vec::new();
    let action_kind = classify(event.kind);
    match action_kind {
        Some(EventAction::Upsert) => {
            for p in &event.paths {
                if let Some(rel) = vault_relative(vault_root, p) {
                    actions.push(WatchAction::Upsert(rel));
                }
            }
        }
        Some(EventAction::Delete) => {
            for p in &event.paths {
                if let Some(rel) = vault_relative(vault_root, p) {
                    actions.push(WatchAction::Delete(rel));
                }
            }
        }
        Some(EventAction::Rename) => {
            // Notify packs (from, to) in event.paths in that order.
            if let Some(from) = event.paths.first()
                && let Some(rel) = vault_relative(vault_root, from)
            {
                actions.push(WatchAction::Delete(rel));
            }
            if let Some(to) = event.paths.get(1)
                && let Some(rel) = vault_relative(vault_root, to)
            {
                actions.push(WatchAction::Upsert(rel));
            }
        }
        None => {}
    }
    actions
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventAction {
    Upsert,
    Delete,
    Rename,
}

fn classify(kind: EventKind) -> Option<EventAction> {
    match kind {
        EventKind::Create(_)
        | EventKind::Modify(
            ModifyKind::Data(_)
            | ModifyKind::Metadata(_)
            | ModifyKind::Any
            | ModifyKind::Name(RenameMode::To),
        )
        | EventKind::Access(AccessKind::Close(AccessMode::Write)) => Some(EventAction::Upsert),
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) | EventKind::Remove(_) => {
            Some(EventAction::Delete)
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => Some(EventAction::Rename),
        // Access(non-close), Modify(Name(Any|Other)), Other, Any -> drop.
        _ => None,
    }
}

/// True if the event signals an inotify queue overflow ([[ADR-003 Filesystem
/// Change Tracking]] / notify's [`Flag::Rescan`]).
#[must_use]
pub fn is_overflow(event: &Event) -> bool {
    event.flag() == Some(Flag::Rescan)
}

/// Collapse a sequence of [`WatchAction`]s into a single batch. The last
/// action per path wins (delete-then-upsert ends up as upsert, and so on).
/// The batch keeps deterministic order by sorting paths inside each list.
#[must_use]
pub fn coalesce(actions: impl IntoIterator<Item = WatchAction>) -> WatchBatch {
    #[derive(Clone, Copy)]
    enum Last {
        Upsert,
        Delete,
    }
    let mut state: BTreeMap<String, Last> = BTreeMap::new();
    for a in actions {
        match a {
            WatchAction::Upsert(p) => {
                state.insert(p, Last::Upsert);
            }
            WatchAction::Delete(p) => {
                state.insert(p, Last::Delete);
            }
        }
    }
    let mut upserts = Vec::new();
    let mut deletes = Vec::new();
    for (path, last) in state {
        match last {
            Last::Upsert => upserts.push(path),
            Last::Delete => deletes.push(path),
        }
    }
    WatchBatch { upserts, deletes }
}

/// Filter and normalize an absolute notify path. Returns the vault-relative
/// forward-slash path if the entry is a `.md` file outside any dot-directory;
/// returns `None` otherwise (so the caller drops the event).
fn vault_relative(vault_root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(vault_root).ok()?;
    if rel.extension().is_none_or(|e| e != "md") {
        return None;
    }
    for component in rel.components() {
        let s = component.as_os_str().to_str()?;
        if s.starts_with('.') {
            return None;
        }
    }
    Some(rel.to_str()?.replace('\\', "/"))
}

/// Resolve a vault-relative path back to an absolute one for re-reading.
#[must_use]
pub fn absolute_path(vault_root: &Path, rel: &str) -> PathBuf {
    vault_root.join(rel)
}

/// Drain notify events from `rx`, coalesce in `window`-length batches, and
/// dispatch each batch to the `index_writer` and (on overflow) a
/// `rescan_now` cast to `vault_scanner`.
///
/// Loops until the channel closes -- that only happens when the watcher
/// process is being shut down and its sender side is dropped.
pub async fn run_debouncer(
    mut rx: UnboundedReceiver<notify::Result<Event>>,
    vault_root: PathBuf,
    window: Duration,
    health: Arc<HealthState>,
) {
    loop {
        let Some(first) = rx.recv().await else {
            break;
        };
        health.record_event(chrono::Utc::now());
        let mut events = vec![first];
        let deadline = Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(e)) => events.push(e),
                Ok(None) | Err(_) => break,
            }
        }
        let mut overflow = false;
        let mut actions = Vec::new();
        for entry in events {
            match entry {
                Ok(ev) => {
                    if is_overflow(&ev) {
                        overflow = true;
                    }
                    actions.extend(map_event(&ev, &vault_root));
                }
                Err(err) => {
                    tracing::warn!(error = %err, "filesystem_watcher: notify error");
                }
            }
        }
        if overflow {
            tracing::warn!("filesystem_watcher: overflow -- dispatching rescan_now");
            VaultScanner::cast(
                Dest::from(VAULT_SCANNER_NAME),
                VaultScannerMessage::RescanNow,
            );
        }
        let batch = coalesce(actions);
        if batch.upserts.is_empty() && batch.deletes.is_empty() {
            continue;
        }
        let index_batch = build_index_batch(&vault_root, batch);
        IndexWriterState::cast(
            Dest::from(INDEX_WRITER_NAME),
            IndexWriterMessage::Batch(index_batch),
        );
    }
}

fn build_index_batch(vault_root: &Path, batch: WatchBatch) -> IndexBatch {
    let mut upserts = Vec::with_capacity(batch.upserts.len());
    for rel in batch.upserts {
        let abs = absolute_path(vault_root, &rel);
        match vault::scan_one(vault_root, &abs) {
            Ok(note) => upserts.push(note),
            Err(kind) => {
                tracing::warn!(
                    path = %abs.display(),
                    error = %kind,
                    "filesystem_watcher: re-read failed; skipping upsert"
                );
            }
        }
    }
    IndexBatch {
        upserts,
        deletes: batch.deletes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, RemoveKind};

    fn vault() -> PathBuf {
        PathBuf::from("/v")
    }

    fn ev(kind: EventKind, paths: &[&str]) -> Event {
        let mut e = Event::new(kind);
        for p in paths {
            e = e.add_path(PathBuf::from(p));
        }
        e
    }

    #[test]
    fn create_md_under_vault_is_upsert() {
        let e = ev(EventKind::Create(CreateKind::File), &["/v/a.md"]);
        assert_eq!(
            map_event(&e, &vault()),
            vec![WatchAction::Upsert("a.md".into())]
        );
    }

    #[test]
    fn non_md_file_is_dropped() {
        let e = ev(EventKind::Create(CreateKind::File), &["/v/a.txt"]);
        assert!(map_event(&e, &vault()).is_empty());
    }

    #[test]
    fn dot_directory_descendants_are_dropped() {
        let e = ev(EventKind::Create(CreateKind::File), &["/v/.obsidian/x.md"]);
        assert!(map_event(&e, &vault()).is_empty());
        let e = ev(
            EventKind::Create(CreateKind::File),
            &["/v/sub/.hidden/y.md"],
        );
        assert!(map_event(&e, &vault()).is_empty());
    }

    #[test]
    fn path_outside_vault_is_dropped() {
        let e = ev(EventKind::Create(CreateKind::File), &["/elsewhere/a.md"]);
        assert!(map_event(&e, &vault()).is_empty());
    }

    #[test]
    fn modify_data_becomes_upsert() {
        let e = ev(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            &["/v/a.md"],
        );
        assert_eq!(
            map_event(&e, &vault()),
            vec![WatchAction::Upsert("a.md".into())]
        );
    }

    #[test]
    fn close_write_becomes_upsert() {
        let e = ev(
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            &["/v/a.md"],
        );
        assert_eq!(
            map_event(&e, &vault()),
            vec![WatchAction::Upsert("a.md".into())]
        );
    }

    #[test]
    fn rename_from_is_delete() {
        let e = ev(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            &["/v/a.md"],
        );
        assert_eq!(
            map_event(&e, &vault()),
            vec![WatchAction::Delete("a.md".into())]
        );
    }

    #[test]
    fn rename_to_is_upsert() {
        let e = ev(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            &["/v/b.md"],
        );
        assert_eq!(
            map_event(&e, &vault()),
            vec![WatchAction::Upsert("b.md".into())]
        );
    }

    #[test]
    fn rename_both_emits_delete_then_upsert() {
        let e = ev(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &["/v/a.md", "/v/b.md"],
        );
        assert_eq!(
            map_event(&e, &vault()),
            vec![
                WatchAction::Delete("a.md".into()),
                WatchAction::Upsert("b.md".into())
            ]
        );
    }

    #[test]
    fn remove_is_delete() {
        let e = ev(EventKind::Remove(RemoveKind::File), &["/v/a.md"]);
        assert_eq!(
            map_event(&e, &vault()),
            vec![WatchAction::Delete("a.md".into())]
        );
    }

    #[test]
    fn coalesce_last_action_per_path_wins() {
        let actions = vec![
            WatchAction::Upsert("a.md".into()),
            WatchAction::Upsert("a.md".into()),
            WatchAction::Delete("b.md".into()),
            WatchAction::Upsert("b.md".into()),
            WatchAction::Delete("c.md".into()),
        ];
        let batch = coalesce(actions);
        assert_eq!(batch.upserts, vec!["a.md".to_string(), "b.md".to_string()]);
        assert_eq!(batch.deletes, vec!["c.md".to_string()]);
    }

    #[test]
    fn overflow_event_returns_empty_actions() {
        // Caller dispatches rescan_now; map_event itself produces no per-path actions.
        let e = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        assert!(is_overflow(&e));
        assert!(map_event(&e, &vault()).is_empty());
    }
}
