//! One-shot local markdown-merge for the `anwesen merge` subcommand ([ANW-27]).
//!
//! Walks a vault directory once (the same [`vault::scan`] as `doctor`),
//! evaluates the `--query` string, and assembles the merged markdown document
//! with [`query::execute_merge`] -- the very engine the HTTP `/query` merge
//! mode ([ANW-26]) uses. CLI and HTTP output are therefore byte-identical for
//! the same vault and query. No HTTP, no watcher, no persistent index.

use std::fmt::Write as _;
use std::path::Path;

use crate::query::{self, MergeError, QueryError};
use crate::store::NoteStore;
use crate::vault::{self, ScanIssue};

/// Why a one-shot merge could not produce a document.
#[derive(Debug)]
pub enum MergeCliError {
    /// The `--query` string did not parse. Same grammar, same message as the
    /// HTTP `/query` `400`.
    Query(QueryError),
    /// One or more files could not be read, or their frontmatter did not
    /// parse. Same hard-failure posture as `doctor`; soft warnings (e.g. a
    /// non-mapping frontmatter root) are ignored, matching what `serve`
    /// ingests.
    Scan(Vec<ScanIssue>),
    /// The `__anw-kind` homogeneity guard rejected the matched set. The
    /// `String` is the same naming message the HTTP path returns as `400`,
    /// surfaced here on stderr instead.
    Kind(String),
}

impl MergeCliError {
    /// Render the error for stderr. One concern per line, deterministic so the
    /// stderr surface is golden-testable.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Query(e) => format!("{e}\n"),
            Self::Scan(issues) => {
                let mut s = String::from("merge: cannot read vault\n");
                for issue in issues {
                    let _ = writeln!(s, "  {}: {}", issue.path.display(), issue.kind);
                }
                s
            }
            Self::Kind(msg) => msg.clone(),
        }
    }
}

/// Walk `vault_root`, evaluate `raw_query`, and return the merged document.
///
/// On success the returned `String` is byte-identical to the HTTP merge body
/// for the same vault and query. An empty match set yields an empty string.
///
/// # Errors
/// - [`MergeCliError::Query`] when `raw_query` is malformed;
/// - [`MergeCliError::Scan`] when the directory is unreadable or any file's
///   frontmatter fails to parse;
/// - [`MergeCliError::Kind`] when the `__anw-kind` homogeneity guard fails.
pub fn run(vault_root: &Path, raw_query: &str) -> Result<String, MergeCliError> {
    let parsed = query::parse(raw_query).map_err(MergeCliError::Query)?;

    let scan = vault::scan(vault_root);
    if !scan.issues.is_empty() {
        return Err(MergeCliError::Scan(scan.issues));
    }

    // The merge engine reads from a NoteStore exactly as the HTTP path does;
    // a one-shot `replace` is the whole "index" this subcommand needs.
    let store = NoteStore::new();
    store.replace(scan.notes);

    query::execute_merge(&store, &parsed)
        .map_err(|MergeError::KindGuard(msg)| MergeCliError::Kind(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    #[test]
    fn merges_bodies_with_source_markers() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\nnum: 1\n---\nalpha\n");
        write(tmp.path(), "b.md", "---\nnum: 2\n---\nbeta\n");
        let out = run(tmp.path(), "").unwrap();
        // Body is frontmatter-stripped; fragments join with a blank line and
        // there is no trailing newline -- byte-identical to the HTTP body.
        assert_eq!(
            out,
            "<!-- source: a.md -->\nalpha\n\n\n<!-- source: b.md -->\nbeta\n"
        );
    }

    #[test]
    fn order_desc_then_path_tiebreak() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\nnum: 1\n---\nlow\n");
        write(tmp.path(), "b.md", "---\nnum: 3\n---\nhigh\n");
        write(tmp.path(), "c.md", "---\nnum: 2\n---\nmid\n");
        let out = run(tmp.path(), "__anw-order=num:desc").unwrap();
        let bodies: Vec<&str> = out
            .lines()
            .filter(|l| !l.starts_with("<!--") && !l.is_empty())
            .collect();
        assert_eq!(bodies, vec!["high", "mid", "low"]);
    }

    #[test]
    fn empty_match_set_is_empty_string() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\ntags: [x]\n---\nbody\n");
        // Predicate matches nothing.
        assert_eq!(run(tmp.path(), "tags=nope").unwrap(), "");
    }

    #[test]
    fn empty_vault_is_empty_string() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(run(tmp.path(), "").unwrap(), "");
    }

    #[test]
    fn kind_guard_passes_when_uniform() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\nkind: skill\n---\nA\n");
        write(tmp.path(), "b.md", "---\nkind: skill\n---\nB\n");
        assert!(run(tmp.path(), "__anw-kind=kind").is_ok());
    }

    #[test]
    fn kind_guard_rejects_distinct_values_naming_offenders() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\nkind: skill\n---\nA\n");
        write(tmp.path(), "b.md", "---\nkind: note\n---\nB\n");
        let err = run(tmp.path(), "__anw-kind=kind").unwrap_err();
        let msg = err.render();
        assert!(msg.contains("distinct values found"));
        assert!(msg.contains("a.md"));
        assert!(msg.contains("b.md"));
    }

    #[test]
    fn kind_guard_rejects_missing_key() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\nkind: skill\n---\nA\n");
        write(tmp.path(), "b.md", "---\n---\nB\n");
        let err = run(tmp.path(), "__anw-kind=kind").unwrap_err();
        assert!(matches!(err, MergeCliError::Kind(_)));
        assert!(err.render().contains("notes missing the key"));
    }

    #[test]
    fn malformed_query_is_reported() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\n---\nbody\n");
        let err = run(tmp.path(), "x__bogus=1").unwrap_err();
        assert!(matches!(err, MergeCliError::Query(_)));
        // Non-empty stderr surface.
        assert!(!err.render().is_empty());
    }

    #[test]
    fn unparseable_frontmatter_is_a_scan_error() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "bad.md", "---\n:: :: ::\n---\n");
        let err = run(tmp.path(), "").unwrap_err();
        assert!(matches!(err, MergeCliError::Scan(_)));
        assert!(err.render().contains("cannot read vault"));
        assert!(err.render().contains("bad.md"));
    }

    #[test]
    fn output_matches_http_engine_for_same_vault_and_query() {
        // The shared-engine check: the CLI path and a directly-built store
        // feeding execute_merge must produce the identical document.
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\nnum: 2\n---\nA body\n");
        write(tmp.path(), "b.md", "---\nnum: 1\n---\nB body\n");
        let raw = "__anw-order=num:asc";

        let cli = run(tmp.path(), raw).unwrap();

        let scan = vault::scan(tmp.path());
        let store = NoteStore::new();
        store.replace(scan.notes);
        let parsed = query::parse(raw).unwrap();
        let direct = query::execute_merge(&store, &parsed).unwrap();

        assert_eq!(cli, direct);
    }
}
