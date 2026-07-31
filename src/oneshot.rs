//! One-shot local query evaluation for the `anwesen merge` ([ANW-27]) and
//! `anwesen query` ([ANW-43]) subcommands.
//!
//! Both walk a vault directory once (the same [`vault::scan`] as `doctor`),
//! evaluate the `--query` string, and hand the resulting store to the very
//! engine the HTTP `/query` endpoint uses: [`query::execute_merge`] for the
//! merged markdown document ([ANW-26]), [`query::execute`] for the JSON
//! projection. CLI and HTTP output are therefore byte-identical for the same
//! vault and query. No HTTP, no watcher, no persistent index.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use crate::query::{self, MergeError, ParsedQuery, QueryError};
use crate::store::NoteStore;
use crate::vault::{self, ScanIssue};

/// Why a one-shot evaluation could not produce a document.
#[derive(Debug)]
pub enum OneshotError {
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
    /// surfaced here on stderr instead. Merge mode only -- the JSON
    /// projection does not run the guard, exactly as the endpoint does not.
    Kind(String),
}

impl OneshotError {
    /// Render the error for stderr. One concern per line, deterministic so the
    /// stderr surface is golden-testable. `subcommand` names the caller so the
    /// scan header reads `merge:` or `query:` as the operator invoked it.
    #[must_use]
    pub fn render(&self, subcommand: &str) -> String {
        match self {
            Self::Query(e) => format!("{e}\n"),
            Self::Scan(issues) => {
                let mut s = format!("{subcommand}: cannot read vault\n");
                for issue in issues {
                    let _ = writeln!(s, "  {}: {}", issue.path.display(), issue.kind);
                }
                s
            }
            Self::Kind(msg) => msg.clone(),
        }
    }
}

/// Parse `raw_query` and load `vault_root` into a one-shot store.
///
/// The single path both subcommands take, so the two cannot drift in how they
/// parse a query or how strictly they read a vault.
fn load(vault_root: &Path, raw_query: &str) -> Result<(ParsedQuery, Arc<NoteStore>), OneshotError> {
    let parsed = query::parse(raw_query).map_err(OneshotError::Query)?;

    let scan = vault::scan(vault_root);
    if !scan.issues.is_empty() {
        return Err(OneshotError::Scan(scan.issues));
    }

    // The engine reads from a NoteStore exactly as the HTTP path does; a
    // one-shot `replace` is the whole "index" these subcommands need.
    let store = NoteStore::new();
    store.replace(scan.notes);
    Ok((parsed, store))
}

/// Walk `vault_root`, evaluate `raw_query`, and return the merged document.
///
/// On success the returned `String` is byte-identical to the HTTP merge body
/// for the same vault and query. An empty match set yields an empty string.
///
/// # Errors
/// - [`OneshotError::Query`] when `raw_query` is malformed;
/// - [`OneshotError::Scan`] when the directory is unreadable or any file's
///   frontmatter fails to parse;
/// - [`OneshotError::Kind`] when the `__anw-kind` homogeneity guard fails.
pub fn merge(vault_root: &Path, raw_query: &str) -> Result<String, OneshotError> {
    let (parsed, store) = load(vault_root, raw_query)?;
    query::execute_merge(&store, &parsed)
        .map_err(|MergeError::KindGuard(msg)| OneshotError::Kind(msg))
}

/// Walk `vault_root`, evaluate `raw_query`, and return the JSON document
/// `GET /query` returns for the same vault and query -- same projection, same
/// `results` / `total` / `truncated` shape, same compact serialization, so a
/// script moving between daemon and CLI needs no second parser ([ANW-43]).
///
/// Result ordering and the `__anw-limit` semantics are the endpoint's,
/// because this is the endpoint's [`query::execute`]: paths ascending, the
/// cap applied after `total` is counted.
///
/// # Errors
/// - [`OneshotError::Query`] when `raw_query` is malformed;
/// - [`OneshotError::Scan`] when the directory is unreadable or any file's
///   frontmatter fails to parse.
///
/// # Panics
/// If the response fails to serialize -- unreachable, as the HTTP handler's
/// identical `to_vec` call also treats it: every field is a string, a number,
/// or frontmatter already converted to `serde_json::Value`.
pub fn query_json(vault_root: &Path, raw_query: &str) -> Result<String, OneshotError> {
    let (parsed, store) = load(vault_root, raw_query)?;
    let response = query::execute(&store, &parsed, query::rfc3339_z);
    Ok(serde_json::to_string(&response).expect("query response serializes"))
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
        let out = merge(tmp.path(), "").unwrap();
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
        let out = merge(tmp.path(), "__anw-order=num:desc").unwrap();
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
        assert_eq!(merge(tmp.path(), "tags=nope").unwrap(), "");
    }

    #[test]
    fn empty_vault_is_empty_string() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(merge(tmp.path(), "").unwrap(), "");
    }

    #[test]
    fn kind_guard_passes_when_uniform() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\nkind: skill\n---\nA\n");
        write(tmp.path(), "b.md", "---\nkind: skill\n---\nB\n");
        assert!(merge(tmp.path(), "__anw-kind=kind").is_ok());
    }

    #[test]
    fn kind_guard_rejects_distinct_values_naming_offenders() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\nkind: skill\n---\nA\n");
        write(tmp.path(), "b.md", "---\nkind: note\n---\nB\n");
        let err = merge(tmp.path(), "__anw-kind=kind").unwrap_err();
        let msg = err.render("merge");
        assert!(msg.contains("distinct values found"));
        assert!(msg.contains("a.md"));
        assert!(msg.contains("b.md"));
    }

    #[test]
    fn kind_guard_rejects_missing_key() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\nkind: skill\n---\nA\n");
        write(tmp.path(), "b.md", "---\n---\nB\n");
        let err = merge(tmp.path(), "__anw-kind=kind").unwrap_err();
        assert!(matches!(err, OneshotError::Kind(_)));
        assert!(err.render("merge").contains("notes missing the key"));
    }

    #[test]
    fn malformed_query_is_reported() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\n---\nbody\n");
        let err = merge(tmp.path(), "x__bogus=1").unwrap_err();
        assert!(matches!(err, OneshotError::Query(_)));
        // Non-empty stderr surface.
        assert!(!err.render("merge").is_empty());
    }

    #[test]
    fn unparseable_frontmatter_is_a_scan_error() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "bad.md", "---\n:: :: ::\n---\n");
        let err = merge(tmp.path(), "").unwrap_err();
        assert!(matches!(err, OneshotError::Scan(_)));
        assert!(err.render("merge").contains("cannot read vault"));
        assert!(err.render("merge").contains("bad.md"));
    }

    #[test]
    fn output_matches_http_engine_for_same_vault_and_query() {
        // The shared-engine check: the CLI path and a directly-built store
        // feeding execute_merge must produce the identical document.
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\nnum: 2\n---\nA body\n");
        write(tmp.path(), "b.md", "---\nnum: 1\n---\nB body\n");
        let raw = "__anw-order=num:asc";

        let cli = merge(tmp.path(), raw).unwrap();

        let scan = vault::scan(tmp.path());
        let store = NoteStore::new();
        store.replace(scan.notes);
        let parsed = query::parse(raw).unwrap();
        let direct = query::execute_merge(&store, &parsed).unwrap();

        assert_eq!(cli, direct);
    }

    // --- `query` (ANW-43) --------------------------------------------------

    fn json(doc: &str) -> serde_json::Value {
        serde_json::from_str(doc).expect("query output parses as JSON")
    }

    #[test]
    fn query_projects_path_frontmatter_and_file_facts() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Projects/alpha.md",
            "---\ntags: [project]\nversion: 2\n---\nbody\n",
        );
        let doc = json(&query_json(tmp.path(), "").unwrap());

        assert_eq!(doc["total"], 1);
        assert_eq!(doc["truncated"], false);
        let row = &doc["results"][0];
        assert_eq!(row["path"], "Projects/alpha.md");
        assert_eq!(row["frontmatter"]["version"], 2);
        assert_eq!(row["frontmatter"]["tags"][0], "project");
        assert_eq!(row["size"], 40);
        // The endpoint's timestamp dialect, seconds precision with a Z suffix.
        let lm = row["last_modified"].as_str().unwrap();
        assert!(lm.ends_with('Z'), "{lm}");
        assert!(row["etag"].as_str().unwrap().starts_with('"'));
        // The body is elided here as it is on the endpoint.
        assert!(row.get("body").is_none());
    }

    #[test]
    fn query_predicates_filter_and_limit_truncates_after_total() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\nstatus: draft\n---\nA\n");
        write(tmp.path(), "b.md", "---\nstatus: draft\n---\nB\n");
        write(tmp.path(), "c.md", "---\nstatus: done\n---\nC\n");

        let filtered = json(&query_json(tmp.path(), "status=draft").unwrap());
        assert_eq!(filtered["total"], 2);
        assert_eq!(filtered["results"].as_array().unwrap().len(), 2);

        let capped = json(&query_json(tmp.path(), "status=draft&__anw-limit=1").unwrap());
        // `total` counts the full match set; the cap keeps the first row.
        assert_eq!(capped["total"], 2);
        assert_eq!(capped["truncated"], true);
        assert_eq!(capped["results"].as_array().unwrap().len(), 1);
        assert_eq!(capped["results"][0]["path"], "a.md");
    }

    #[test]
    fn query_empty_match_set_is_an_empty_result_list() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\ntags: [x]\n---\nbody\n");
        let doc = json(&query_json(tmp.path(), "tags=nope").unwrap());
        assert_eq!(doc["total"], 0);
        assert_eq!(doc["truncated"], false);
        assert!(doc["results"].as_array().unwrap().is_empty());
    }

    #[test]
    fn query_malformed_query_is_reported_naming_the_subcommand() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\n---\nbody\n");
        let err = query_json(tmp.path(), "x__bogus=1").unwrap_err();
        assert!(matches!(err, OneshotError::Query(_)));
        assert!(!err.render("query").is_empty());
    }

    #[test]
    fn query_unparseable_frontmatter_is_a_scan_error() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "bad.md", "---\n:: :: ::\n---\n");
        let err = query_json(tmp.path(), "").unwrap_err();
        assert!(err.render("query").starts_with("query: cannot read vault"));
        assert!(err.render("query").contains("bad.md"));
    }

    #[test]
    fn query_kind_guard_does_not_apply() {
        // `__anw-kind` guards merge output only. Parsing it must not make the
        // JSON path fail on a mixed set, matching the endpoint.
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\nkind: skill\n---\nA\n");
        write(tmp.path(), "b.md", "---\nkind: note\n---\nB\n");
        assert!(merge(tmp.path(), "__anw-kind=kind").is_err());
        let doc = json(&query_json(tmp.path(), "__anw-kind=kind").unwrap());
        assert_eq!(doc["total"], 2);
    }

    #[test]
    fn query_output_matches_the_http_projection_byte_for_byte() {
        // The shared-engine check for the JSON surface: same store, same
        // `execute`, same serialization as the `/query` handler.
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\nnum: 2\n---\nA body\n");
        write(tmp.path(), "b.md", "---\nnum: 1\n---\nB body\n");
        let raw = "__anw-limit=1";

        let cli = query_json(tmp.path(), raw).unwrap();

        let scan = vault::scan(tmp.path());
        let store = NoteStore::new();
        store.replace(scan.notes);
        let parsed = query::parse(raw).unwrap();
        let direct =
            serde_json::to_string(&query::execute(&store, &parsed, crate::query::rfc3339_z))
                .unwrap();

        assert_eq!(cli, direct);
    }
}
