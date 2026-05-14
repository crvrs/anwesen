//! `anwesen doctor` base checks per [ANW-18]. Walks the vault once,
//! collects hard failures and soft warnings from [`crate::vault::scan`],
//! plus any HTTP-surface path collisions that scrape past the OS path
//! distinction (e.g. Unicode normalization differences). Returns a
//! [`Report`] the binary renders to stdout; a non-empty report exits
//! non-zero per the User Manual.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

use crate::vault::{self, ScanIssue, ScanWarning};

#[derive(Debug, Default)]
pub struct Report {
    pub note_count: usize,
    pub issues: Vec<ScanIssue>,
    pub warnings: Vec<ScanWarning>,
    pub path_collisions: Vec<PathCollision>,
}

#[derive(Debug)]
pub struct PathCollision {
    pub path: String,
    pub count: usize,
}

impl Report {
    /// True if any anomaly was found -- the contract for the doctor exit
    /// code per the User Manual.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty() && self.warnings.is_empty() && self.path_collisions.is_empty()
    }
}

/// Run `doctor`'s base checks on the given vault root. Pure -- the binary
/// is responsible for rendering the report and choosing an exit code.
#[must_use]
pub fn run(vault_root: &Path) -> Report {
    let scan = vault::scan(vault_root);
    let collisions = detect_path_collisions(
        &scan
            .notes
            .iter()
            .map(|n| n.path.clone())
            .collect::<Vec<_>>(),
    );
    Report {
        note_count: scan.notes.len(),
        issues: scan.issues,
        warnings: scan.warnings,
        path_collisions: collisions,
    }
}

/// Find HTTP-surface path collisions: two distinct OS files whose
/// vault-relative, forward-slash-normalized paths are equal. Almost
/// never bites on Linux (the OS already enforces unique paths), but
/// Unicode-NFC vs -NFD or symlink-stitched paths can in principle hit it.
#[must_use]
pub fn detect_path_collisions(paths: &[String]) -> Vec<PathCollision> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for p in paths {
        *counts.entry(p.as_str()).or_insert(0) += 1;
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

/// Render the report to a multi-line string the binary writes to stdout.
#[must_use]
pub fn render(vault_root: &Path, report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "anwesen doctor: {}", vault_root.display());
    let _ = writeln!(out, "  notes:           {}", report.note_count);
    let _ = writeln!(out, "  issues:          {}", report.issues.len());
    let _ = writeln!(out, "  warnings:        {}", report.warnings.len());
    let _ = writeln!(out, "  path collisions: {}", report.path_collisions.len());
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
        let dups =
            detect_path_collisions(&["a.md".into(), "b.md".into(), "a.md".into(), "a.md".into()]);
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
}
