//! `anwesen doctor` checks per [ANW-18] (base) and [ANW-9] (type drift).
//! Walks the vault once, collects hard failures and soft warnings from
//! [`crate::vault::scan`], plus any HTTP-surface path collisions and any
//! frontmatter type drift (same key carrying incompatible shapes across
//! notes per [[ADR-005 Frontmatter Contract Type Coercion and Cross-Note
//! Shapes]]). Returns a [`Report`] the binary renders to stdout; a
//! non-empty report exits non-zero per the User Manual.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

use crate::vault::{self, Note, ScanIssue, ScanWarning, Value};

/// Number of sample paths retained per (key, shape) pair in
/// [`TypeDrift`]. Three is a balance between giving the operator a
/// concrete starting point and keeping the doctor report copy/paste-able.
const DRIFT_SAMPLES_PER_SHAPE: usize = 3;

#[derive(Debug, Default)]
pub struct Report {
    pub note_count: usize,
    pub issues: Vec<ScanIssue>,
    pub warnings: Vec<ScanWarning>,
    pub path_collisions: Vec<PathCollision>,
    pub type_drifts: Vec<TypeDrift>,
}

#[derive(Debug)]
pub struct PathCollision {
    pub path: String,
    pub count: usize,
}

/// One frontmatter key observed with incompatible shapes across notes.
/// `shapes` is sorted by shape name for stable rendering.
#[derive(Debug)]
pub struct TypeDrift {
    pub key: String,
    pub shapes: Vec<DriftShape>,
}

#[derive(Debug)]
pub struct DriftShape {
    /// Shape name as classified by [`shape_of`] -- one of `"null"`,
    /// `"bool"`, `"number"`, `"string"`, `"date"`, `"list"`, `"mapping"`.
    pub shape: &'static str,
    pub count: usize,
    /// Up to [`DRIFT_SAMPLES_PER_SHAPE`] vault-relative paths exhibiting
    /// this shape, in the order they were scanned.
    pub samples: Vec<String>,
}

impl Report {
    /// True if any anomaly was found -- the contract for the doctor exit
    /// code per the User Manual.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
            && self.warnings.is_empty()
            && self.path_collisions.is_empty()
            && self.type_drifts.is_empty()
    }
}

/// Run `doctor`'s checks on the given vault root. Pure -- the binary is
/// responsible for rendering the report and choosing an exit code.
#[must_use]
pub fn run(vault_root: &Path) -> Report {
    let scan = vault::scan(vault_root);
    let paths: Vec<&str> = scan.notes.iter().map(|n| n.path.as_str()).collect();
    let collisions = detect_path_collisions(&paths);
    let type_drifts = detect_type_drift(&scan.notes);
    Report {
        note_count: scan.notes.len(),
        issues: scan.issues,
        warnings: scan.warnings,
        path_collisions: collisions,
        type_drifts,
    }
}

/// Find HTTP-surface path collisions: two distinct OS files whose
/// vault-relative, forward-slash-normalized paths are equal byte-for-byte.
/// Enforces the byte-equality invariant only; case-insensitive or
/// Unicode-normalized (NFC vs NFD) collisions are out of scope per
/// [[ADR-003 Filesystem Change Tracking]]'s Linux-only v1 target, and
/// `follow_links(false)` keeps symlink-stitched paths out of the input.
#[must_use]
pub fn detect_path_collisions(paths: &[&str]) -> Vec<PathCollision> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for p in paths {
        *counts.entry(*p).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter(|(_, c)| *c > 1)
        .map(|(p, c)| PathCollision {
            path: p.to_string(),
            count: c,
        })
        .collect()
}

/// Detect frontmatter type drift across notes. Each top-level frontmatter
/// key is grouped by shape (per [`shape_of`]); a key carrying more than
/// one shape is reported with per-shape counts and a small sample of paths
/// per shape. Top-level only -- nested keys are addressable in queries via
/// `author.name=...` but flagging drift on every nested path is out of
/// proportion for v1; revisit if a consumer asks.
#[must_use]
pub fn detect_type_drift(notes: &[Note]) -> Vec<TypeDrift> {
    // key -> shape -> (count, samples)
    let mut by_key: BTreeMap<&str, BTreeMap<&'static str, (usize, Vec<String>)>> = BTreeMap::new();
    for note in notes {
        for (key, value) in &note.frontmatter {
            let shape = shape_of(value);
            let entry = by_key
                .entry(key.as_str())
                .or_default()
                .entry(shape)
                .or_insert_with(|| (0, Vec::new()));
            entry.0 += 1;
            if entry.1.len() < DRIFT_SAMPLES_PER_SHAPE {
                entry.1.push(note.path.clone());
            }
        }
    }
    by_key
        .into_iter()
        .filter(|(_, shapes)| shapes.len() > 1)
        .map(|(key, shapes)| TypeDrift {
            key: key.to_string(),
            shapes: shapes
                .into_iter()
                .map(|(shape, (count, samples))| DriftShape {
                    shape,
                    count,
                    samples,
                })
                .collect(),
        })
        .collect()
}

/// Classify a [`Value`] into a stable shape name for drift comparison.
/// `Int` and `Float` collapse to `"number"`; `Date` and `DateTime`
/// collapse to `"date"` -- the User Manual's "Types and shapes" section
/// treats both as the same date contract, so a key parsing as date-only
/// in some notes and datetime in others is not drift. A scalar coerced to
/// a date in one note but left as a string in another *is* drift, per
/// ADR-005's "string vs date" example.
#[must_use]
pub fn shape_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Int(_) | Value::Float(_) => "number",
        Value::String(_) => "string",
        Value::Date(_) | Value::DateTime(_) => "date",
        Value::Sequence(_) => "list",
        Value::Mapping(_) => "mapping",
    }
}

/// Render the report to a multi-line string the binary writes to stdout.
#[must_use]
pub fn render(vault_root: &Path, report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "anwesen doctor: {}", vault_root.display());
    let _ = writeln!(out, "  notes:           {}", report.note_count);
    let _ = writeln!(out, "  issues:          {}", report.issues.len());
    let _ = writeln!(out, "  warnings:        {}", report.warnings.len());
    let _ = writeln!(out, "  path collisions: {}", report.path_collisions.len());
    let _ = writeln!(out, "  type drift:      {}", report.type_drifts.len());
    if !report.issues.is_empty() {
        out.push_str("\nissues:\n");
        for issue in &report.issues {
            let _ = writeln!(out, "  {}: {}", issue.path.display(), issue.kind);
        }
    }
    if !report.warnings.is_empty() {
        out.push_str("\nwarnings:\n");
        for w in &report.warnings {
            let _ = writeln!(out, "  {}: {}", w.path.display(), w.kind);
        }
    }
    if !report.path_collisions.is_empty() {
        out.push_str("\npath collisions:\n");
        for c in &report.path_collisions {
            let _ = writeln!(out, "  {} ({} files)", c.path, c.count);
        }
    }
    if !report.type_drifts.is_empty() {
        out.push_str("\ntype drift:\n");
        for d in &report.type_drifts {
            let _ = writeln!(out, "  {}:", d.key);
            for s in &d.shapes {
                let _ = writeln!(out, "    {} ({} notes)", s.shape, s.count);
                for path in &s.samples {
                    let _ = writeln!(out, "      {path}");
                }
            }
        }
    }
    if report.is_clean() {
        out.push_str("\nOK.\n");
    } else {
        out.push_str("\nFAIL.\n");
    }
    out
}

/// Convenience: run + render against an absolute vault path. Returns the
/// rendered report and the exit code the binary should propagate.
#[must_use]
pub fn run_and_render(vault_root: &Path) -> (String, i32) {
    let report = run(vault_root);
    let exit = i32::from(!report.is_clean());
    (render(vault_root, &report), exit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    #[test]
    fn clean_vault_reports_clean() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\ntags: [demo]\n---\nbody\n");
        let r = run(tmp.path());
        assert!(r.is_clean(), "expected clean: {r:?}");
        assert_eq!(r.note_count, 1);
    }

    #[test]
    fn malformed_frontmatter_reports_issue_not_warning() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "bad.md", "---\nkey: : :\n---\n");
        let r = run(tmp.path());
        assert!(!r.is_clean());
        assert_eq!(r.note_count, 0);
        assert_eq!(r.issues.len(), 1);
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn frontmatter_not_mapping_reports_warning_keeps_note() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "list.md", "---\n- a\n- b\n---\nbody\n");
        let r = run(tmp.path());
        // Note is kept (serve compatibility); doctor flags the warning.
        assert_eq!(r.note_count, 1);
        assert!(r.issues.is_empty());
        assert_eq!(r.warnings.len(), 1);
        assert!(!r.is_clean());
    }

    #[test]
    fn detect_path_collisions_finds_duplicates() {
        let dups = detect_path_collisions(&["a.md", "b.md", "a.md", "a.md"]);
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].path, "a.md");
        assert_eq!(dups[0].count, 3);
    }

    #[test]
    fn run_and_render_exits_zero_on_clean() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\n---\n");
        let (out, exit) = run_and_render(tmp.path());
        assert_eq!(exit, 0);
        assert!(out.contains("OK."));
        assert!(out.contains("notes:           1"));
    }

    #[test]
    fn run_and_render_exits_nonzero_on_issue() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "bad.md", "---\n:: :: ::\n---\n");
        let (out, exit) = run_and_render(tmp.path());
        assert_eq!(exit, 1);
        assert!(out.contains("FAIL."));
        assert!(out.contains("issues:"));
    }

    #[test]
    fn unreadable_file_reports_io_issue() {
        // Lock the report header's "unreadable file" advertised behavior:
        // mode-zero file surfaces as ScanIssueKind::Io, not a silent skip.
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "good.md", "---\n---\n");
        let bad = tmp.path().join("bad.md");
        fs::write(&bad, "---\n---\n").unwrap();
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();
        // Skip the assertion when mode 000 is not enforced (running as
        // root, or a filesystem that ignores Unix permission bits).
        if fs::read(&bad).is_ok() {
            fs::set_permissions(&bad, fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }
        let r = run(tmp.path());
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(r.note_count, 1, "good.md still scanned");
        assert_eq!(r.issues.len(), 1, "bad.md surfaces as an Io issue");
        assert!(matches!(r.issues[0].kind, vault::ScanIssueKind::Io(_)));
        assert!(!r.is_clean());
    }

    #[test]
    fn type_drift_flags_scalar_vs_list() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "scalar.md", "---\ntags: python\n---\n");
        write(tmp.path(), "list.md", "---\ntags: [python, go]\n---\n");
        let r = run(tmp.path());
        assert_eq!(r.note_count, 2);
        assert_eq!(r.type_drifts.len(), 1);
        let drift = &r.type_drifts[0];
        assert_eq!(drift.key, "tags");
        assert_eq!(drift.shapes.len(), 2);
        let shapes: BTreeMap<&str, usize> =
            drift.shapes.iter().map(|s| (s.shape, s.count)).collect();
        assert_eq!(shapes.get("string"), Some(&1));
        assert_eq!(shapes.get("list"), Some(&1));
        assert!(!r.is_clean());
    }

    #[test]
    fn type_drift_flags_string_vs_date() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\ndate: 2026-05-14\n---\n");
        write(tmp.path(), "b.md", "---\ndate: not-a-date\n---\n");
        let r = run(tmp.path());
        assert_eq!(r.type_drifts.len(), 1);
        let drift = &r.type_drifts[0];
        assert_eq!(drift.key, "date");
        let shapes: BTreeMap<&str, usize> =
            drift.shapes.iter().map(|s| (s.shape, s.count)).collect();
        assert_eq!(shapes.get("date"), Some(&1));
        assert_eq!(shapes.get("string"), Some(&1));
    }

    #[test]
    fn type_drift_flags_number_vs_string() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "n.md", "---\nversion: 3\n---\n");
        write(tmp.path(), "s.md", "---\nversion: \"3.0-rc1\"\n---\n");
        let r = run(tmp.path());
        assert_eq!(r.type_drifts.len(), 1);
        assert_eq!(r.type_drifts[0].key, "version");
    }

    #[test]
    fn date_only_and_datetime_do_not_drift() {
        // Date and DateTime both classify as "date" -- the User Manual's
        // "Types and shapes" section treats them as one date contract.
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\nwhen: 2026-05-14\n---\n");
        write(tmp.path(), "b.md", "---\nwhen: 2026-05-14T10:14:22Z\n---\n");
        let r = run(tmp.path());
        assert!(
            r.type_drifts.is_empty(),
            "no drift expected: {:?}",
            r.type_drifts
        );
        assert!(r.is_clean());
    }

    #[test]
    fn int_and_float_do_not_drift() {
        // Both collapse to "number" -- a key written as 3 in one note and
        // 3.14 in another is not a vault-author error worth flagging.
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "i.md", "---\nscore: 3\n---\n");
        write(tmp.path(), "f.md", "---\nscore: 3.14\n---\n");
        let r = run(tmp.path());
        assert!(r.type_drifts.is_empty());
    }

    #[test]
    fn type_drift_caps_sample_paths_per_shape() {
        let tmp = TempDir::new().unwrap();
        // Five string and five list notes; only DRIFT_SAMPLES_PER_SHAPE
        // (3) sample paths should be retained per shape, but the count
        // should reflect all of them.
        for i in 0..5 {
            write(tmp.path(), &format!("s{i}.md"), "---\nkind: solo\n---\n");
            write(tmp.path(), &format!("l{i}.md"), "---\nkind: [solo]\n---\n");
        }
        let r = run(tmp.path());
        assert_eq!(r.type_drifts.len(), 1);
        let drift = &r.type_drifts[0];
        for s in &drift.shapes {
            assert_eq!(s.count, 5);
            assert_eq!(s.samples.len(), DRIFT_SAMPLES_PER_SHAPE);
        }
    }

    #[test]
    fn type_drift_render_includes_key_and_samples() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.md", "---\ntags: python\n---\n");
        write(tmp.path(), "b.md", "---\ntags: [python]\n---\n");
        let (out, exit) = run_and_render(tmp.path());
        assert_eq!(exit, 1);
        assert!(out.contains("type drift:"));
        assert!(out.contains("tags:"));
        assert!(out.contains("a.md"));
        assert!(out.contains("b.md"));
    }
}
