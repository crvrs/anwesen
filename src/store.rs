//! In-memory note store. Holds the authoritative `vault::Note` record set
//! that the scanner produces and the watcher maintains; consumed by the
//! HTTP layer ([ANW-13] / [ANW-14] / [ANW-15]) for read-one and listing
//! responses.
//!
//! Per [ANW-11], the daemon rereads from disk only on a watcher event for
//! that path; this store is that in-memory cache and -- per
//! [[ADR-009 Reverse ADR-002 In-Memory Evaluation No Tantivy]] -- the sole
//! authoritative record set both `/notes` and `/query` read from.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use crate::vault::Note;

/// Shared, thread-safe handle to the note set keyed by vault-relative path.
#[derive(Debug, Default)]
pub struct NoteStore {
    inner: RwLock<BTreeMap<String, Note>>,
}

impl NoteStore {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Replace the entire record set. Used at startup and after overflow
    /// recovery.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` has been poisoned by a panic in a writer.
    /// Anwesen does not call user code under the lock, so this is unreachable
    /// in practice.
    pub fn replace(&self, notes: Vec<Note>) {
        let mut guard = self.inner.write().expect("note_store: write lock poisoned");
        guard.clear();
        for note in notes {
            guard.insert(note.path.clone(), note);
        }
    }

    /// Apply one debounce-window's worth of changes. Mirrors the order in
    /// [`crate::index::NoteIndex::apply_batch`]: deletes first, then upserts.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` has been poisoned.
    pub fn apply_batch(&self, upserts: Vec<Note>, deletes: &[String]) {
        let mut guard = self.inner.write().expect("note_store: write lock poisoned");
        for path in deletes {
            guard.remove(path);
        }
        for note in upserts {
            guard.insert(note.path.clone(), note);
        }
    }

    /// # Panics
    /// Panics if the inner `RwLock` has been poisoned.
    pub fn upsert(&self, note: Note) {
        let mut guard = self.inner.write().expect("note_store: write lock poisoned");
        guard.insert(note.path.clone(), note);
    }

    /// # Panics
    /// Panics if the inner `RwLock` has been poisoned.
    pub fn delete(&self, path: &str) {
        let mut guard = self.inner.write().expect("note_store: write lock poisoned");
        guard.remove(path);
    }

    /// Clone a single record by path, or `None` if absent. The read lock is
    /// held only for the lookup; the returned `Note` is independent.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` has been poisoned.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<Note> {
        let guard = self.inner.read().expect("note_store: read lock poisoned");
        guard.get(path).cloned()
    }

    /// # Panics
    /// Panics if the inner `RwLock` has been poisoned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .expect("note_store: read lock poisoned")
            .len()
    }

    /// Run `f` with a shared reference to the underlying map. The read lock
    /// is held for the call; do not run anything that might block on a
    /// writer here. Used by the folder-listing handler in [`crate::http`].
    ///
    /// # Panics
    /// Panics if the inner `RwLock` has been poisoned.
    pub fn with_read<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&BTreeMap<String, Note>) -> R,
    {
        let guard = self.inner.read().expect("note_store: read lock poisoned");
        f(&guard)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use std::collections::BTreeMap as Bm;

    fn note(path: &str) -> Note {
        Note {
            path: path.into(),
            frontmatter: Bm::new(),
            body: String::new(),
            raw_bytes: b"hi".to_vec(),
            last_modified: DateTime::from_timestamp(0, 0).unwrap(),
            etag: "\"abc\"".into(),
            size: 2,
        }
    }

    #[test]
    fn replace_clears_previous() {
        let s = NoteStore::new();
        s.replace(vec![note("a.md"), note("b.md")]);
        assert_eq!(s.len(), 2);
        s.replace(vec![note("c.md")]);
        assert_eq!(s.len(), 1);
        assert!(s.get("a.md").is_none());
        assert!(s.get("c.md").is_some());
    }

    #[test]
    fn apply_batch_deletes_then_upserts() {
        let s = NoteStore::new();
        s.replace(vec![note("a.md"), note("b.md")]);
        s.apply_batch(vec![note("c.md")], &["a.md".to_string()]);
        assert_eq!(s.len(), 2);
        assert!(s.get("a.md").is_none());
        assert!(s.get("b.md").is_some());
        assert!(s.get("c.md").is_some());
    }

    #[test]
    fn upsert_replaces_by_path() {
        let s = NoteStore::new();
        s.upsert(note("a.md"));
        let mut n2 = note("a.md");
        n2.etag = "\"def\"".into();
        s.upsert(n2);
        assert_eq!(s.get("a.md").unwrap().etag, "\"def\"");
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn get_unknown_path_is_none() {
        let s = NoteStore::new();
        assert!(s.get("nope.md").is_none());
    }
}
