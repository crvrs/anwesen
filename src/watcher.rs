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
///
/// The `*Tree` variants carry a directory instead of a note. Native events
/// name only the directory when one is created, removed, or renamed; the
/// files under it produce no events of their own [ANW-36].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchAction {
    Upsert(String),
    Delete(String),
    UpsertTree(String),
    DeleteTree(String),
}

/// One debouncer window's worth of coalesced changes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WatchBatch {
    pub upserts: Vec<String>,
    pub deletes: Vec<String>,
    /// Directories to walk for notes to upsert.
    pub upsert_trees: Vec<String>,
    /// Directories whose indexed notes are all gone.
    pub delete_trees: Vec<String>,
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
        Some(EventAction::Modify) => {
            // Content and metadata events only ever name a file.
            for p in &event.paths {
                if let Some(rel) = note_relative(vault_root, p) {
                    actions.push(WatchAction::Upsert(rel));
                }
            }
        }
        Some(EventAction::Appear) => {
            for p in &event.paths {
                actions.extend(appear(vault_root, p));
            }
        }
        Some(EventAction::Vanish) => {
            for p in &event.paths {
                actions.extend(vanish(vault_root, p));
            }
        }
        Some(EventAction::Rename) => {
            // Notify packs (from, to) in event.paths in that order.
            if let Some(from) = event.paths.first() {
                actions.extend(vanish(vault_root, from));
            }
            if let Some(to) = event.paths.get(1) {
                actions.extend(appear(vault_root, to));
            }
        }
        Some(EventAction::RenameUnpaired) => {
            // FSEvents (macOS) cannot pair the two sides of a rename and
            // reports `Modify(Name(Any))` for each side separately, so the
            // direction has to come off the filesystem [ANW-36].
            for p in &event.paths {
                if p.exists() {
                    actions.extend(appear(vault_root, p));
                } else {
                    actions.extend(vanish(vault_root, p));
                }
            }
            // `p.exists()` can only report the moment it is asked. A path
            // that vanishes right after the probe is caught downstream:
            // `build_index_batch` turns a not-found upsert into a delete.
        }
        None => {}
    }
    actions
}

/// A path that now exists: a note to read, or a directory to walk. The
/// filesystem answers which, so a non-note file (an attachment, an editor
/// temp file) costs nothing beyond the probe.
fn appear(vault_root: &Path, abs: &Path) -> Option<WatchAction> {
    if abs.is_dir() {
        tree_relative(vault_root, abs).map(WatchAction::UpsertTree)
    } else {
        note_relative(vault_root, abs).map(WatchAction::Upsert)
    }
}

/// A path that is gone: a note to drop, or a directory whose notes are all
/// gone with it. The filesystem cannot be asked -- it no longer holds the
/// entry -- so both are emitted and the index decides which one matches.
///
/// A `.md` suffix does not prove the path was a file: a directory may carry
/// it too, and then the suffix test alone strands its notes in the index
/// [ANW-40]. The two key sets are disjoint -- the note delete removes the key
/// `dir`, the prefix delete removes keys under `dir/` -- so emitting both is
/// always safe. It costs one range scan per deleted note.
fn vanish(vault_root: &Path, abs: &Path) -> Vec<WatchAction> {
    let mut actions = Vec::new();
    if let Some(rel) = note_relative(vault_root, abs) {
        actions.push(WatchAction::Delete(rel));
    }
    if let Some(rel) = tree_relative(vault_root, abs) {
        actions.push(WatchAction::DeleteTree(rel));
    }
    actions
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventAction {
    /// Existing file, new content.
    Modify,
    /// Path came into the vault.
    Appear,
    /// Path left the vault.
    Vanish,
    /// Both sides in one event, `(from, to)`.
    Rename,
    /// One side of a rename, direction unknown.
    RenameUnpaired,
}

fn classify(kind: EventKind) -> Option<EventAction> {
    match kind {
        EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Metadata(_) | ModifyKind::Any)
        | EventKind::Access(AccessKind::Close(AccessMode::Write)) => Some(EventAction::Modify),
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            Some(EventAction::Appear)
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) | EventKind::Remove(_) => {
            Some(EventAction::Vanish)
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => Some(EventAction::Rename),
        EventKind::Modify(ModifyKind::Name(RenameMode::Any | RenameMode::Other)) => {
            Some(EventAction::RenameUnpaired)
        }
        // Access(non-close), Other, Any -> drop.
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
/// action per path wins (delete-then-upsert ends up as upsert, and so on);
/// notes and directories are tracked separately, since a directory action
/// covers paths a note action cannot name. The batch keeps deterministic
/// order by sorting paths inside each list.
#[must_use]
pub fn coalesce(actions: impl IntoIterator<Item = WatchAction>) -> WatchBatch {
    #[derive(Clone, Copy)]
    enum Last {
        Upsert,
        Delete,
    }
    let mut notes: BTreeMap<String, Last> = BTreeMap::new();
    let mut trees: BTreeMap<String, Last> = BTreeMap::new();
    for a in actions {
        match a {
            WatchAction::Upsert(p) => {
                notes.insert(p, Last::Upsert);
            }
            WatchAction::Delete(p) => {
                notes.insert(p, Last::Delete);
            }
            WatchAction::UpsertTree(p) => {
                trees.insert(p, Last::Upsert);
            }
            WatchAction::DeleteTree(p) => {
                trees.insert(p, Last::Delete);
            }
        }
    }
    let mut batch = WatchBatch::default();
    for (path, last) in notes {
        match last {
            Last::Upsert => batch.upserts.push(path),
            Last::Delete => batch.deletes.push(path),
        }
    }
    for (path, last) in trees {
        match last {
            Last::Upsert => batch.upsert_trees.push(path),
            Last::Delete => batch.delete_trees.push(path),
        }
    }
    batch
}

fn is_markdown(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "md")
}

/// Filter and normalize an absolute notify path. Returns the vault-relative
/// forward-slash path if the entry is a `.md` file outside any dot-directory;
/// returns `None` otherwise (so the caller drops the event).
fn note_relative(vault_root: &Path, abs: &Path) -> Option<String> {
    if !is_markdown(abs) {
        return None;
    }
    relative(vault_root, abs)
}

/// Same, for a directory: any path that is not a note. The vault root itself
/// relativizes to the empty string and is dropped -- a root-level event is a
/// vault-wide signal the rescan path handles, not a prefix to delete.
fn tree_relative(vault_root: &Path, abs: &Path) -> Option<String> {
    let rel = relative(vault_root, abs)?;
    if rel.is_empty() { None } else { Some(rel) }
}

fn relative(vault_root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(vault_root).ok()?;
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
        if batch.upserts.is_empty()
            && batch.deletes.is_empty()
            && batch.upsert_trees.is_empty()
            && batch.delete_trees.is_empty()
        {
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
    let mut deletes = batch.deletes;
    for rel in batch.upserts {
        let abs = absolute_path(vault_root, &rel);
        match vault::scan_one(vault_root, &abs) {
            Ok(note) => upserts.push(note),
            // The file is gone by the time we read it. An event backend that
            // coalesces create-and-remove into one upsert-shaped event would
            // otherwise leave the entry in the index forever [ANW-36].
            Err(vault::ScanIssueKind::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                deletes.push(rel);
            }
            Err(kind) => {
                tracing::warn!(
                    path = %abs.display(),
                    error = %kind,
                    "filesystem_watcher: re-read failed; skipping upsert"
                );
            }
        }
    }
    // A walked directory carries its own prefix delete: the index under that
    // prefix must match what the walk found. Without it, a directory renamed
    // out and another renamed in within one window coalesces to a bare
    // `UpsertTree` -- the delete is lost and the old notes outlive their
    // files [ANW-36].
    let mut delete_prefixes = batch.delete_trees;
    delete_prefixes.extend(batch.upsert_trees.iter().cloned());
    for dir in &batch.upsert_trees {
        let abs = absolute_path(vault_root, dir);
        let result = vault::scan_from(vault_root, &abs);
        for issue in &result.issues {
            tracing::warn!(
                path = %issue.path.display(),
                error = %issue.kind,
                "filesystem_watcher: subtree walk issue; skipping note"
            );
        }
        upserts.extend(result.notes);
    }
    IndexBatch {
        upserts,
        deletes,
        delete_prefixes,
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
            vec![
                WatchAction::Delete("a.md".into()),
                WatchAction::DeleteTree("a.md".into())
            ]
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
                WatchAction::DeleteTree("a.md".into()),
                WatchAction::Upsert("b.md".into())
            ]
        );
    }

    #[test]
    fn remove_is_delete() {
        let e = ev(EventKind::Remove(RemoveKind::File), &["/v/a.md"]);
        assert_eq!(
            map_event(&e, &vault()),
            vec![
                WatchAction::Delete("a.md".into()),
                WatchAction::DeleteTree("a.md".into())
            ]
        );
    }

    #[test]
    fn removed_md_directory_drops_the_notes_under_it() {
        // A directory may be named `*.md` too. The vanished path cannot be
        // probed, so the note delete and the prefix delete both go out and
        // the index applies whichever matches [ANW-40].
        let e = ev(EventKind::Remove(RemoveKind::Folder), &["/v/Archive.md"]);
        assert_eq!(
            map_event(&e, &vault()),
            vec![
                WatchAction::Delete("Archive.md".into()),
                WatchAction::DeleteTree("Archive.md".into())
            ]
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
    fn coalesce_keeps_notes_and_trees_apart() {
        let actions = vec![
            WatchAction::Upsert("Notes/a.md".into()),
            WatchAction::DeleteTree("Notes".into()),
            WatchAction::UpsertTree("Fresh".into()),
        ];
        let batch = coalesce(actions);
        assert_eq!(batch.upserts, vec!["Notes/a.md".to_string()]);
        assert!(batch.deletes.is_empty());
        assert_eq!(batch.upsert_trees, vec!["Fresh".to_string()]);
        assert_eq!(batch.delete_trees, vec!["Notes".to_string()]);
    }

    #[test]
    fn removed_directory_is_a_tree_delete() {
        let e = ev(EventKind::Remove(RemoveKind::Folder), &["/v/Notes"]);
        assert_eq!(
            map_event(&e, &vault()),
            vec![WatchAction::DeleteTree("Notes".into())]
        );
    }

    #[test]
    fn directory_renamed_away_is_a_tree_delete() {
        let e = ev(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            &["/v/Notes"],
        );
        assert_eq!(
            map_event(&e, &vault()),
            vec![WatchAction::DeleteTree("Notes".into())]
        );
    }

    #[test]
    fn vault_root_itself_is_not_a_tree_action() {
        let e = ev(EventKind::Remove(RemoveKind::Folder), &["/v"]);
        assert!(map_event(&e, &vault()).is_empty());
    }

    #[test]
    fn removed_non_note_file_touches_no_note() {
        // A vanished path with no `.md` suffix is treated as a directory;
        // the prefix simply matches nothing in the store.
        let e = ev(EventKind::Remove(RemoveKind::File), &["/v/image.png"]);
        assert_eq!(
            map_event(&e, &vault()),
            vec![WatchAction::DeleteTree("image.png".into())]
        );
    }

    #[test]
    fn created_directory_is_a_tree_upsert() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("Notes")).unwrap();
        let e = ev(EventKind::Create(CreateKind::Folder), &[]).add_path(root.path().join("Notes"));
        assert_eq!(
            map_event(&e, root.path()),
            vec![WatchAction::UpsertTree("Notes".into())]
        );
    }

    #[test]
    fn unpaired_rename_resolves_by_existence() {
        // FSEvents (macOS) reports each side of a rename as
        // `Modify(Name(Any))` with no way to pair them.
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("here.md"), "x").unwrap();
        let kind = EventKind::Modify(ModifyKind::Name(RenameMode::Any));

        let present = ev(kind, &[]).add_path(root.path().join("here.md"));
        assert_eq!(
            map_event(&present, root.path()),
            vec![WatchAction::Upsert("here.md".into())]
        );

        let absent = ev(kind, &[]).add_path(root.path().join("gone.md"));
        assert_eq!(
            map_event(&absent, root.path()),
            vec![
                WatchAction::Delete("gone.md".into()),
                WatchAction::DeleteTree("gone.md".into())
            ]
        );
    }

    #[test]
    fn upsert_of_a_vanished_file_becomes_a_delete() {
        let root = tempfile::tempdir().unwrap();
        let batch = WatchBatch {
            upserts: vec!["gone.md".to_string()],
            ..WatchBatch::default()
        };
        let index_batch = build_index_batch(root.path(), batch);
        assert!(index_batch.upserts.is_empty());
        assert_eq!(index_batch.deletes, vec!["gone.md".to_string()]);
    }

    #[test]
    fn tree_upsert_walks_the_subtree() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("Notes/deep")).unwrap();
        std::fs::write(root.path().join("Notes/a.md"), "---\nk: 1\n---\nbody\n").unwrap();
        std::fs::write(root.path().join("Notes/deep/b.md"), "body\n").unwrap();
        std::fs::write(root.path().join("Notes/skip.txt"), "no\n").unwrap();
        std::fs::write(root.path().join("outside.md"), "no\n").unwrap();

        let batch = WatchBatch {
            upsert_trees: vec!["Notes".to_string()],
            ..WatchBatch::default()
        };
        let index_batch = build_index_batch(root.path(), batch);
        let mut paths: Vec<String> = index_batch.upserts.into_iter().map(|n| n.path).collect();
        paths.sort();
        assert_eq!(paths, vec!["Notes/a.md", "Notes/deep/b.md"]);
    }

    #[test]
    fn tree_upsert_carries_its_prefix_delete() {
        // One directory renamed out and another renamed in within one window
        // coalesces to a bare `UpsertTree`; the walk must still drop whatever
        // the old directory left in the index [ANW-36].
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("Notes")).unwrap();
        std::fs::write(root.path().join("Notes/new.md"), "body\n").unwrap();

        let batch = coalesce([
            WatchAction::DeleteTree("Notes".to_string()),
            WatchAction::UpsertTree("Notes".to_string()),
        ]);
        assert!(batch.delete_trees.is_empty());

        let index_batch = build_index_batch(root.path(), batch);
        assert_eq!(index_batch.delete_prefixes, vec!["Notes".to_string()]);
        let paths: Vec<String> = index_batch.upserts.into_iter().map(|n| n.path).collect();
        assert_eq!(paths, vec!["Notes/new.md"]);
    }

    #[test]
    fn overflow_event_returns_empty_actions() {
        // Caller dispatches rescan_now; map_event itself produces no per-path actions.
        let e = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        assert!(is_overflow(&e));
        assert!(map_event(&e, &vault()).is_empty());
    }
}
