//! In-memory Tantivy index over note frontmatter.
//!
//! Implements [[ADR-002 Tantivy as Frontmatter Index]] + the ANW-12 schema:
//!
//! - one `json` field (`frontmatter`) with the `raw` tokenizer pinned for v1
//!   (exact-match semantics on every key, matches the keyword-style operator
//!   surface in the User Manual);
//! - one `string` field (`path`) with the `raw` tokenizer (exact lookup +
//!   anchored prefix queries).
//!
//! The index lives in a [`RamDirectory`](tantivy::directory::RamDirectory) --
//! rebuilt at startup from the scanner output and maintained incrementally
//! by [`upsert`](NoteIndex::upsert) / [`delete`](NoteIndex::delete) calls
//! driven from the filesystem watcher in [ANW-16].

use anyhow::{Context, Result};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use tantivy::schema::{Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions};
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

use crate::vault::{Note, Value};

/// 50 MB heap for the writer. Tantivy's documented minimum is 15 MB; this
/// sits comfortably above that for low-thousands-of-notes vaults without
/// bloating idle memory.
const WRITER_HEAP_BYTES: usize = 50 * 1024 * 1024;

/// Strongly-typed handle to the two schema fields. Carried alongside the
/// [`Index`] so call sites don't restring field names.
#[derive(Debug, Clone, Copy)]
pub struct Fields {
    pub path: Field,
    pub frontmatter: Field,
}

/// The in-memory note index. Owns the [`Index`] and a long-lived
/// [`IndexWriter`]; one `NoteIndex` per daemon.
pub struct NoteIndex {
    fields: Fields,
    index: Index,
    writer: IndexWriter,
}

impl NoteIndex {
    /// Construct an empty in-memory index with the pinned v1 schema.
    ///
    /// # Errors
    /// Returns the underlying Tantivy error if writer allocation fails (the
    /// only failure mode reachable for a fresh `RamDirectory`).
    pub fn new() -> Result<Self> {
        let mut schema_builder = Schema::builder();

        let raw_indexing = TextFieldIndexing::default()
            .set_tokenizer("raw")
            .set_index_option(IndexRecordOption::Basic);

        let path_options = TextOptions::default()
            .set_indexing_options(raw_indexing.clone())
            .set_stored();

        let path = schema_builder.add_text_field("path", path_options);

        // JSON field: open-schema frontmatter. `raw` tokenizer pins exact-match
        // semantics on every key, per the open question closed in
        // [[ADR-002 Tantivy as Frontmatter Index]].
        let json_options = tantivy::schema::JsonObjectOptions::default()
            .set_indexing_options(raw_indexing)
            .set_stored();
        let frontmatter = schema_builder.add_json_field("frontmatter", json_options);

        let schema = schema_builder.build();
        let index = Index::create_in_ram(schema);
        let writer: IndexWriter = index
            .writer(WRITER_HEAP_BYTES)
            .context("create tantivy writer")?;

        Ok(Self {
            fields: Fields { path, frontmatter },
            index,
            writer,
        })
    }

    /// Discard every document and reindex the given notes. Used at startup
    /// and during overflow recovery per [[ADR-003 Filesystem Change Tracking]].
    ///
    /// # Errors
    /// Returns the underlying Tantivy error if any add/commit fails.
    pub fn rebuild(&mut self, notes: &[Note]) -> Result<()> {
        self.writer
            .delete_all_documents()
            .context("delete_all_documents")?;
        for note in notes {
            self.add_document(note)?;
        }
        self.writer.commit().context("commit rebuild")?;
        Ok(())
    }

    /// Insert-or-replace one note by path. Subsequent reads through a fresh
    /// reader will see the new content after this call returns.
    ///
    /// # Errors
    /// Returns the underlying Tantivy error if the add or commit fails.
    pub fn upsert(&mut self, note: &Note) -> Result<()> {
        self.writer.delete_term(self.path_term(&note.path));
        self.add_document(note)?;
        self.writer.commit().context("commit upsert")?;
        Ok(())
    }

    /// Drop one note by path. No-op (still commits) if the path was never
    /// indexed; callers can call freely on delete-events.
    ///
    /// # Errors
    /// Returns the underlying Tantivy error if the commit fails.
    pub fn delete(&mut self, path: &str) -> Result<()> {
        self.writer.delete_term(self.path_term(path));
        self.writer.commit().context("commit delete")?;
        Ok(())
    }

    /// Number of indexed documents from a fresh reader. Read-only; intended
    /// for tests and the `/health` endpoint ([ANW-8]).
    ///
    /// # Errors
    /// Returns the underlying Tantivy error if opening the reader fails.
    pub fn document_count(&self) -> Result<u64> {
        let reader = self.index.reader().context("open reader")?;
        let searcher = reader.searcher();
        Ok(searcher.num_docs())
    }

    /// Expose the [`Index`] so other layers (the future query handler in
    /// [ANW-15]) can build their own readers and parsers without re-deriving
    /// the schema.
    #[must_use]
    pub fn index(&self) -> &Index {
        &self.index
    }

    #[must_use]
    pub fn fields(&self) -> Fields {
        self.fields
    }

    fn path_term(&self, path: &str) -> Term {
        Term::from_field_text(self.fields.path, path)
    }

    fn add_document(&mut self, note: &Note) -> Result<()> {
        // Build the doc as JSON keyed by schema field name and let Tantivy
        // parse it -- avoids restating the OwnedValue tree by hand.
        let doc_json = json!({
            "path": &note.path,
            "frontmatter": frontmatter_to_json(&note.frontmatter),
        });
        let doc = TantivyDocument::parse_json(&self.index.schema(), &doc_json.to_string())
            .context("parse_json")?;
        self.writer.add_document(doc).context("add_document")?;
        Ok(())
    }
}

/// Convert a typed [`Value`] tree to `serde_json::Value`. Typed dates and
/// datetimes are emitted as their ISO-8601 / RFC 3339 string forms so they
/// sort correctly under range queries with the `raw` tokenizer.
fn value_to_json(v: &Value) -> JsonValue {
    match v {
        Value::Null => JsonValue::Null,
        Value::Bool(b) => JsonValue::Bool(*b),
        Value::Int(i) => json!(i),
        Value::Float(f) => json!(f),
        Value::String(s) => JsonValue::String(s.clone()),
        Value::Date(d) => JsonValue::String(d.format("%Y-%m-%d").to_string()),
        Value::DateTime(dt) => JsonValue::String(dt.to_rfc3339()),
        Value::Sequence(seq) => JsonValue::Array(seq.iter().map(value_to_json).collect()),
        Value::Mapping(m) => {
            let mut map = JsonMap::new();
            for (k, v) in m {
                map.insert(k.clone(), value_to_json(v));
            }
            JsonValue::Object(map)
        }
    }
}

fn frontmatter_to_json(fm: &crate::vault::Frontmatter) -> JsonValue {
    let mut map = JsonMap::new();
    for (k, v) in fm {
        map.insert(k.clone(), value_to_json(v));
    }
    JsonValue::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use std::collections::BTreeMap;
    use tantivy::collector::TopDocs;
    use tantivy::query::TermQuery;

    fn sample_note(path: &str, tag: &str) -> Note {
        let mut fm: BTreeMap<String, Value> = BTreeMap::new();
        fm.insert(
            "tags".into(),
            Value::Sequence(vec![Value::String(tag.into())]),
        );
        fm.insert("title".into(), Value::String(format!("note {path}")));
        Note {
            path: path.into(),
            frontmatter: fm,
            body: String::new(),
            raw_bytes: Vec::new(),
            last_modified: Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap(),
            etag: "\"deadbeef\"".into(),
            size: 0,
        }
    }

    fn count_path(idx: &NoteIndex, path: &str) -> tantivy::Result<usize> {
        let reader = idx.index().reader()?;
        let searcher = reader.searcher();
        let term = Term::from_field_text(idx.fields().path, path);
        let query = TermQuery::new(term, IndexRecordOption::Basic);
        let hits = searcher.search(&query, &TopDocs::with_limit(10))?;
        Ok(hits.len())
    }

    #[test]
    fn rebuild_populates_index() {
        let mut idx = NoteIndex::new().unwrap();
        idx.rebuild(&[sample_note("a.md", "x"), sample_note("b.md", "y")])
            .unwrap();
        assert_eq!(idx.document_count().unwrap(), 2);
        assert_eq!(count_path(&idx, "a.md").unwrap(), 1);
        assert_eq!(count_path(&idx, "b.md").unwrap(), 1);
    }

    #[test]
    fn rebuild_replaces_previous_contents() {
        let mut idx = NoteIndex::new().unwrap();
        idx.rebuild(&[sample_note("old.md", "x")]).unwrap();
        idx.rebuild(&[sample_note("new.md", "y")]).unwrap();
        assert_eq!(idx.document_count().unwrap(), 1);
        assert_eq!(count_path(&idx, "old.md").unwrap(), 0);
        assert_eq!(count_path(&idx, "new.md").unwrap(), 1);
    }

    #[test]
    fn upsert_replaces_by_path() {
        let mut idx = NoteIndex::new().unwrap();
        idx.upsert(&sample_note("a.md", "v1")).unwrap();
        // Mutate then upsert under the same path; document count must stay 1.
        idx.upsert(&sample_note("a.md", "v2")).unwrap();
        assert_eq!(idx.document_count().unwrap(), 1);
        assert_eq!(count_path(&idx, "a.md").unwrap(), 1);
    }

    #[test]
    fn delete_removes_path() {
        let mut idx = NoteIndex::new().unwrap();
        idx.upsert(&sample_note("a.md", "x")).unwrap();
        idx.upsert(&sample_note("b.md", "y")).unwrap();
        idx.delete("a.md").unwrap();
        assert_eq!(idx.document_count().unwrap(), 1);
        assert_eq!(count_path(&idx, "a.md").unwrap(), 0);
        assert_eq!(count_path(&idx, "b.md").unwrap(), 1);
    }

    #[test]
    fn delete_unknown_path_is_noop() {
        let mut idx = NoteIndex::new().unwrap();
        idx.upsert(&sample_note("a.md", "x")).unwrap();
        idx.delete("never-indexed.md").unwrap();
        assert_eq!(idx.document_count().unwrap(), 1);
    }

    #[test]
    fn value_to_json_coerces_dates_to_iso_strings() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 5, 14).unwrap();
        assert_eq!(
            value_to_json(&Value::Date(d)),
            JsonValue::String("2026-05-14".into())
        );

        let dt = chrono::DateTime::parse_from_rfc3339("2026-05-14T10:14:22Z").unwrap();
        assert_eq!(
            value_to_json(&Value::DateTime(dt)),
            JsonValue::String("2026-05-14T10:14:22+00:00".into())
        );
    }

    #[test]
    fn value_to_json_handles_nested_structures() {
        let mut inner: BTreeMap<String, Value> = BTreeMap::new();
        inner.insert("name".into(), Value::String("brn".into()));
        let v = Value::Mapping(inner);
        let j = value_to_json(&v);
        let JsonValue::Object(obj) = j else {
            panic!("expected object");
        };
        assert_eq!(obj.get("name"), Some(&JsonValue::String("brn".into())));
    }
}
