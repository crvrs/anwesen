//! Vault scanner.
//!
//! Walks the vault root, parses YAML frontmatter, coerces ISO-8601 / RFC 3339
//! date strings to typed dates per [[ADR-005 Frontmatter Contract Type
//! Coercion and Cross-Note Shapes]], retains each file's raw bytes in the
//! [`Note`] record, and computes the strong `ETag` as `BLAKE3` of those raw
//! bytes per [[ADR-006 `ETag` Derivation]].
//!
//! Files that are not Markdown (extension `.md`) and entries under any
//! dot-directory (`.obsidian/`, `.git/`, `.trash/`, ...) are skipped per
//! [[ADR-003 Filesystem Change Tracking]].
//!
//! Per-file failures (I/O, non-UTF-8 path, malformed YAML) are surfaced as
//! [`ScanIssue`]s alongside successful records so callers (the scanner main
//! loop, `doctor`) can decide whether to skip or report.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, NaiveDate, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use thiserror::Error;
use walkdir::WalkDir;

/// One scanned note. `raw_bytes` is retained so `Accept: text/markdown` can
/// return the exact bytes that produced `etag` without re-reading from disk
/// between watcher events ([ANW-13](https://crvrs.youtrack.cloud/issue/ANW-13)).
///
/// Derives `Serialize` / `Deserialize` so the record can flow over Hydra
/// process messages (see [[ADR-004 Hydra as Process Runtime]]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    /// Path relative to the vault root, forward-slash separated.
    pub path: String,
    pub frontmatter: Frontmatter,
    pub body: String,
    pub raw_bytes: Vec<u8>,
    pub last_modified: DateTime<Utc>,
    pub etag: String,
    pub size: u64,
}

pub type Frontmatter = BTreeMap<String, Value>;

/// Frontmatter value with the type-coercion contract from
/// [[ADR-005 Frontmatter Contract Type Coercion and Cross-Note Shapes]] applied.
///
/// The derived `Serialize` / `Deserialize` is used for Hydra messaging
/// (binary-format internal transport). HTTP responses must emit the flat,
/// YAML-natural shape promised by the User Manual; call [`Value::to_json`]
/// for that.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Date(NaiveDate),
    DateTime(DateTime<FixedOffset>),
    Sequence(Vec<Value>),
    Mapping(BTreeMap<String, Value>),
}

impl Value {
    /// Convert to a plain [`serde_json::Value`] tree -- the form the User
    /// Manual promises HTTP consumers (e.g. `"tags": ["a", "b"]` rather than
    /// `{"Sequence": [{"String": "a"}, ...]}`). Typed dates and datetimes
    /// emit as their ISO-8601 / RFC 3339 string forms so they sort correctly
    /// under range queries against the index.
    #[must_use]
    pub fn to_json(&self) -> JsonValue {
        match self {
            Value::Null => JsonValue::Null,
            Value::Bool(b) => JsonValue::Bool(*b),
            Value::Int(i) => json!(i),
            Value::Float(f) => json!(f),
            Value::String(s) => JsonValue::String(s.clone()),
            Value::Date(d) => JsonValue::String(d.format("%Y-%m-%d").to_string()),
            // Canonicalize to UTC with a `Z` suffix so two notes with the
            // same instant but different source offsets serialize identically
            // and compare equal under range queries on the index.
            Value::DateTime(dt) => JsonValue::String(
                dt.with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
            ),
            Value::Sequence(seq) => JsonValue::Array(seq.iter().map(Value::to_json).collect()),
            Value::Mapping(m) => {
                let mut map = JsonMap::new();
                for (k, v) in m {
                    map.insert(k.clone(), v.to_json());
                }
                JsonValue::Object(map)
            }
        }
    }
}

/// Convert a whole [`Frontmatter`] tree to a [`serde_json::Value`] object
/// suitable for HTTP responses.
#[must_use]
pub fn frontmatter_to_json(fm: &Frontmatter) -> JsonValue {
    let mut map = JsonMap::new();
    for (k, v) in fm {
        map.insert(k.clone(), v.to_json());
    }
    JsonValue::Object(map)
}

#[derive(Debug)]
pub struct ScanResult {
    pub notes: Vec<Note>,
    /// Hard failures -- file could not be loaded at all.
    pub issues: Vec<ScanIssue>,
    /// Soft anomalies -- file loaded but flagged for `doctor`. `serve`
    /// ignores these; ANW-18 surfaces them.
    pub warnings: Vec<ScanWarning>,
}

#[derive(Debug)]
pub struct ScanIssue {
    pub path: PathBuf,
    pub kind: ScanIssueKind,
}

#[derive(Debug, Error)]
pub enum ScanIssueKind {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("path is not valid UTF-8")]
    NonUtf8Path,
    #[error("frontmatter YAML parse failed: {0}")]
    FrontmatterParse(#[from] serde_yaml::Error),
    #[error("file body is not valid UTF-8")]
    NonUtf8Body,
}

#[derive(Debug)]
pub struct ScanWarning {
    pub path: PathBuf,
    pub kind: ScanWarningKind,
}

#[derive(Debug, Clone, Error)]
pub enum ScanWarningKind {
    /// Top-level YAML was syntactically valid but not a mapping
    /// (e.g., a stray top-level list). `serve` keeps the note with an
    /// empty frontmatter; `doctor` reports it.
    #[error("frontmatter root is not a YAML mapping")]
    FrontmatterNotMapping,
}

/// Walk the vault and return every readable Markdown note alongside any
/// per-file issues. The walk never panics on a single broken file.
#[must_use]
pub fn scan(vault_root: &Path) -> ScanResult {
    scan_from(vault_root, vault_root)
}

/// Walk `start` (a directory inside `vault_root`) and return every readable
/// Markdown note under it. Note paths stay relative to `vault_root`, so a
/// subtree result is directly comparable with a full [`scan`]. Used by the
/// watcher when a directory appears or is renamed into the vault [ANW-36]:
/// the native event names the directory, never its files.
#[must_use]
pub fn scan_from(vault_root: &Path, start: &Path) -> ScanResult {
    let mut notes = Vec::new();
    let mut issues = Vec::new();
    let mut warnings = Vec::new();

    let walker = WalkDir::new(start)
        .follow_links(false)
        .into_iter()
        // The root entry itself may have a dot-prefixed name (e.g., a
        // `/tmp/.tmpXXXX` tempdir or a vault under a hidden parent); only
        // apply the dot-prefix skip to descendants.
        .filter_entry(|e| e.depth() == 0 || !is_dot_prefixed(e.file_name()));

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                issues.push(ScanIssue {
                    path: err.path().map_or_else(PathBuf::new, Path::to_path_buf),
                    kind: ScanIssueKind::Io(io::Error::other(err.to_string())),
                });
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let abs_path = entry.path();
        if !is_markdown(abs_path) {
            continue;
        }
        match scan_one_audit(vault_root, abs_path) {
            Ok((note, maybe_warning)) => {
                if let Some(kind) = maybe_warning {
                    warnings.push(ScanWarning {
                        path: abs_path.to_path_buf(),
                        kind,
                    });
                }
                notes.push(note);
            }
            Err(kind) => issues.push(ScanIssue {
                path: abs_path.to_path_buf(),
                kind,
            }),
        }
    }

    ScanResult {
        notes,
        issues,
        warnings,
    }
}

fn is_markdown(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "md")
}

fn is_dot_prefixed(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|s| s.starts_with('.'))
}

/// Read a single Markdown file off disk and produce its [`Note`]. Used by
/// the filesystem watcher's per-event handler -- which discards any
/// soft-warning surface.
///
/// # Errors
/// Returns a [`ScanIssueKind`] when the file cannot be read, the body is not
/// valid UTF-8, the path itself isn't UTF-8, or the frontmatter YAML fails to
/// parse.
pub fn scan_one(vault_root: &Path, abs_path: &Path) -> Result<Note, ScanIssueKind> {
    scan_one_audit(vault_root, abs_path).map(|(note, _)| note)
}

/// Read a single Markdown file and surface both the [`Note`] and any
/// soft warning (currently [`ScanWarningKind::FrontmatterNotMapping`]).
/// Used by [`scan`] so `doctor` can report the diagnostic without
/// changing what `serve` ingests.
///
/// # Errors
/// Same as [`scan_one`].
pub fn scan_one_audit(
    vault_root: &Path,
    abs_path: &Path,
) -> Result<(Note, Option<ScanWarningKind>), ScanIssueKind> {
    let raw_bytes = std::fs::read(abs_path)?;
    let metadata = std::fs::metadata(abs_path)?;
    // Size from the bytes we actually hashed -- avoids the one-frame drift
    // possible between `read` and `metadata` if the file is rewritten under
    // us. ETag and size now reflect the same snapshot.
    let size = raw_bytes.len() as u64;
    let last_modified: DateTime<Utc> = metadata.modified()?.into();
    let etag = format!("\"{}\"", blake3::hash(&raw_bytes).to_hex());

    let text = std::str::from_utf8(&raw_bytes).map_err(|_| ScanIssueKind::NonUtf8Body)?;
    let (frontmatter_yaml, body) = split_frontmatter(text);
    let (frontmatter, warning) = parse_frontmatter_audit(frontmatter_yaml)?;

    let rel = abs_path
        .strip_prefix(vault_root)
        .unwrap_or(abs_path)
        .to_path_buf();
    let path = rel.to_str().ok_or(ScanIssueKind::NonUtf8Path)?.to_string();
    // Normalize separators for HTTP-facing storage; on Linux this is a no-op.
    let path = path.replace('\\', "/");

    Ok((
        Note {
            path,
            frontmatter,
            body: body.to_string(),
            raw_bytes,
            last_modified,
            etag,
            size,
        },
        warning,
    ))
}

/// Split a Markdown source into `(frontmatter_yaml, body)`. The frontmatter
/// block is the region delimited by a leading `---\n` and a closing line of
/// exactly `---`. Files without a frontmatter block yield `("", entire body)`.
fn split_frontmatter(src: &str) -> (&str, &str) {
    let Some(after_open) = src.strip_prefix("---\n") else {
        return ("", src);
    };
    // Find a line consisting of exactly "---" (followed by \n or EOF).
    let mut search_start = 0;
    while let Some(rel_idx) = after_open[search_start..].find("\n---") {
        let abs = search_start + rel_idx + 1; // position of the "---"
        let after_close = &after_open[abs + 3..];
        // Accept either a trailing newline or end-of-file after the closing "---".
        if after_close.is_empty() || after_close.starts_with('\n') {
            let yaml = &after_open[..abs];
            let body = after_close.strip_prefix('\n').unwrap_or(after_close);
            return (yaml, body);
        }
        search_start = abs + 3;
    }
    // Open marker but no close: treat whole file as body so the note is still
    // served. A `doctor` follow-up can surface this; for now, do not lose data.
    ("", src)
}

fn parse_frontmatter_audit(
    yaml: &str,
) -> Result<(Frontmatter, Option<ScanWarningKind>), ScanIssueKind> {
    if yaml.trim().is_empty() {
        return Ok((BTreeMap::new(), None));
    }
    let raw: serde_yaml::Value = serde_yaml::from_str(yaml)?;
    // A frontmatter that is not a mapping is not what Obsidian writes;
    // serve keeps an empty frontmatter so the note is still served, but
    // doctor sees the warning so a user can fix the file.
    let serde_yaml::Value::Mapping(map) = raw else {
        return Ok((
            BTreeMap::new(),
            Some(ScanWarningKind::FrontmatterNotMapping),
        ));
    };
    let mut out = BTreeMap::new();
    for (k, v) in map {
        let key = match k {
            serde_yaml::Value::String(s) => s,
            other => yaml_scalar_to_string(&other),
        };
        out.insert(key, coerce(v));
    }
    Ok((out, None))
}

fn coerce(v: serde_yaml::Value) -> Value {
    match v {
        serde_yaml::Value::Null => Value::Null,
        serde_yaml::Value::Bool(b) => Value::Bool(b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::String(n.to_string())
            }
        }
        serde_yaml::Value::String(s) => coerce_string(s),
        serde_yaml::Value::Sequence(seq) => Value::Sequence(seq.into_iter().map(coerce).collect()),
        serde_yaml::Value::Mapping(m) => {
            let mut nested = BTreeMap::new();
            for (k, v) in m {
                let key = match k {
                    serde_yaml::Value::String(s) => s,
                    other => yaml_scalar_to_string(&other),
                };
                nested.insert(key, coerce(v));
            }
            Value::Mapping(nested)
        }
        serde_yaml::Value::Tagged(t) => coerce(t.value),
    }
}

/// Coerce a YAML string to a typed [`Value`]. ISO-8601 date (`YYYY-MM-DD`)
/// and RFC 3339 datetimes -- the shapes Obsidian writes from its date
/// property -- become typed dates; anything else stays a string. See
/// [[ADR-005 Frontmatter Contract Type Coercion and Cross-Note Shapes]].
fn coerce_string(s: String) -> Value {
    if let Ok(d) = NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
        return Value::Date(d);
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(&s) {
        return Value::DateTime(dt);
    }
    Value::String(s)
}

fn yaml_scalar_to_string(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Null => "null".to_string(),
        _ => serde_yaml::to_string(v)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_note(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    #[test]
    fn skips_non_markdown_and_dot_directories() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write_note(root, "kept.md", "---\ntags: [a]\n---\nbody\n");
        write_note(root, "skipped.txt", "not markdown");
        write_note(root, ".obsidian/workspace.json", "{}");
        write_note(root, ".git/HEAD", "ref: refs/heads/main");
        write_note(root, ".trash/old.md", "---\n---\n");
        write_note(root, "nested/.hidden/secret.md", "---\n---\n");

        let result = scan(root);
        let paths: Vec<&str> = result.notes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(paths, vec!["kept.md"]);
        assert!(result.issues.is_empty(), "issues: {:?}", result.issues);
    }

    #[test]
    fn parses_frontmatter_and_separates_body() {
        let tmp = TempDir::new().unwrap();
        write_note(
            tmp.path(),
            "x.md",
            "---\ntitle: Hello\ntags: [a, b]\n---\nthe body\n",
        );
        let result = scan(tmp.path());
        assert_eq!(result.notes.len(), 1);
        let n = &result.notes[0];
        assert_eq!(n.path, "x.md");
        assert_eq!(n.body, "the body\n");
        assert_eq!(
            n.frontmatter.get("title"),
            Some(&Value::String("Hello".into()))
        );
        let tags = n.frontmatter.get("tags").expect("tags present");
        match tags {
            Value::Sequence(s) => {
                assert_eq!(s.len(), 2);
                assert_eq!(s[0], Value::String("a".into()));
            }
            _ => panic!("tags should be a sequence: {tags:?}"),
        }
    }

    #[test]
    fn coerces_iso_dates_and_rfc3339_datetimes() {
        let tmp = TempDir::new().unwrap();
        write_note(
            tmp.path(),
            "d.md",
            "---\ndate: 2026-05-14\nstamp: 2026-05-14T10:14:22Z\nplain: not-a-date\n---\n",
        );
        let result = scan(tmp.path());
        assert_eq!(result.notes.len(), 1);
        let fm = &result.notes[0].frontmatter;
        match fm.get("date").unwrap() {
            Value::Date(d) => {
                assert_eq!(d.format("%Y-%m-%d").to_string(), "2026-05-14");
            }
            other => panic!("expected Date: {other:?}"),
        }
        match fm.get("stamp").unwrap() {
            Value::DateTime(dt) => {
                assert_eq!(dt.to_rfc3339(), "2026-05-14T10:14:22+00:00");
            }
            other => panic!("expected DateTime: {other:?}"),
        }
        assert_eq!(
            fm.get("plain"),
            Some(&Value::String("not-a-date".into())),
            "non-date strings stay strings"
        );
    }

    #[test]
    fn etag_is_blake3_of_raw_bytes() {
        let tmp = TempDir::new().unwrap();
        let src = "---\ntags: [a]\n---\nbody\n";
        write_note(tmp.path(), "x.md", src);
        let result = scan(tmp.path());
        let n = &result.notes[0];
        let expected = format!("\"{}\"", blake3::hash(src.as_bytes()).to_hex());
        assert_eq!(n.etag, expected);
        assert_eq!(n.raw_bytes, src.as_bytes());
        assert_eq!(n.size, src.len() as u64);
    }

    #[test]
    fn malformed_frontmatter_becomes_scan_issue() {
        let tmp = TempDir::new().unwrap();
        // ':' without value and an unterminated list both make this invalid YAML.
        write_note(tmp.path(), "bad.md", "---\ntags: [a, b\nkey: : :\n---\n");
        let result = scan(tmp.path());
        assert!(result.notes.is_empty());
        assert_eq!(result.issues.len(), 1);
        assert!(matches!(
            result.issues[0].kind,
            ScanIssueKind::FrontmatterParse(_)
        ));
    }

    #[test]
    fn file_without_frontmatter_block_is_kept() {
        let tmp = TempDir::new().unwrap();
        write_note(tmp.path(), "plain.md", "just a body, no frontmatter\n");
        let result = scan(tmp.path());
        assert_eq!(result.notes.len(), 1);
        let n = &result.notes[0];
        assert!(n.frontmatter.is_empty());
        assert_eq!(n.body, "just a body, no frontmatter\n");
    }

    #[test]
    fn relative_paths_are_forward_slash_separated() {
        let tmp = TempDir::new().unwrap();
        write_note(tmp.path(), "Projects/anwesen/note.md", "---\n---\n");
        let result = scan(tmp.path());
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].path, "Projects/anwesen/note.md");
    }

    #[test]
    fn split_frontmatter_handles_closing_at_eof() {
        // Closing "---" right at end of file, no trailing newline.
        let (yaml, body) = split_frontmatter("---\nkey: val\n---");
        assert_eq!(yaml, "key: val\n");
        assert_eq!(body, "");
    }

    #[test]
    fn split_frontmatter_no_close_falls_back_to_body() {
        // Open marker but no close means the file is malformed; do not lose data.
        let (yaml, body) = split_frontmatter("---\nkey: val\nno close here\n");
        assert!(yaml.is_empty());
        assert!(body.starts_with("---\n"));
    }

    #[test]
    fn value_to_json_coerces_dates_to_iso_strings() {
        let d = NaiveDate::from_ymd_opt(2026, 5, 14).unwrap();
        assert_eq!(
            Value::Date(d).to_json(),
            JsonValue::String("2026-05-14".into())
        );

        let dt = DateTime::parse_from_rfc3339("2026-05-14T10:14:22Z").unwrap();
        assert_eq!(
            Value::DateTime(dt).to_json(),
            JsonValue::String("2026-05-14T10:14:22Z".into())
        );
        // Different source offset, same instant -> same canonical form.
        let dt2 = DateTime::parse_from_rfc3339("2026-05-14T12:14:22+02:00").unwrap();
        assert_eq!(
            Value::DateTime(dt).to_json(),
            Value::DateTime(dt2).to_json()
        );
    }

    #[test]
    fn value_to_json_emits_flat_yaml_natural_shape() {
        // The User Manual contract: tags: [a, b] -> JSON ["a", "b"], not the
        // tagged-enum form `{"Sequence": [{"String": "a"}, ...]}`.
        let v = Value::Sequence(vec![Value::String("a".into()), Value::String("b".into())]);
        let j = v.to_json();
        assert_eq!(j, json!(["a", "b"]));
    }

    #[test]
    fn value_to_json_handles_nested_structures() {
        let mut inner: BTreeMap<String, Value> = BTreeMap::new();
        inner.insert("name".into(), Value::String("brn".into()));
        let v = Value::Mapping(inner);
        let j = v.to_json();
        let JsonValue::Object(obj) = j else {
            panic!("expected object");
        };
        assert_eq!(obj.get("name"), Some(&JsonValue::String("brn".into())));
    }
}
