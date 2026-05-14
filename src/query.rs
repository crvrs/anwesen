//! `/query` parsing + execution for [ANW-15].
//!
//! The User Manual exposes:
//!
//! - field predicates with operator suffixes (`__in`, `__all`, `__not`,
//!   `__exists`, `__regex`, `__prefix`, `__gt`/`__gte`/`__lt`/`__lte`);
//! - control parameters under the `__anw-` namespace
//!   (`__anw-recursive`, `__anw-path`, `__anw-limit`).
//!
//! Different field predicates AND together; multiple values under
//! `__in` / `__all` are comma-separated; unknown operators are `400`.
//!
//! Predicates are evaluated by iterating the in-memory [`NoteStore`] and
//! applying each [`Predicate::matches`] in turn. At the documented scale
//! (low-thousands-of-notes vaults) this is sub-millisecond; see
//! [[ADR-009 Reverse ADR-002 In-Memory Evaluation No Tantivy]] for the
//! call to keep evaluation in-memory rather than carrying a Tantivy index.

use chrono::{DateTime, NaiveDate};
use regex::Regex;
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::store::NoteStore;
use crate::vault::{Frontmatter, Value, frontmatter_to_json};

#[derive(Debug, PartialEq, Eq)]
pub enum QueryError {
    UnknownOperator(String),
    BadValue(String),
    BadControl(String),
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOperator(s) => write!(f, "unknown operator: {s}"),
            Self::BadValue(s) => write!(f, "bad value: {s}"),
            Self::BadControl(s) => write!(f, "bad control parameter: {s}"),
        }
    }
}

impl std::error::Error for QueryError {}

/// Operator suffix attached to a field predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    /// Exact equality; "array contains" if the frontmatter field is a list.
    Eq,
    /// `__in` -- one-of (any listed value matches).
    In,
    /// `__all` -- array contains every listed value.
    All,
    /// `__not` -- negate exact equality.
    Not,
    /// `__exists` -- presence (`true`) or absence (`false`) of the key.
    Exists,
    /// `__regex` -- regex over a scalar value.
    Regex,
    /// `__prefix` -- string prefix.
    Prefix,
    Gt,
    Gte,
    Lt,
    Lte,
}

#[derive(Debug, Clone)]
pub struct Predicate {
    pub field: String,
    pub op: Operator,
    pub value: String,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedQuery {
    pub predicates: Vec<Predicate>,
    /// `true` by default; `__anw-recursive=false` limits results to direct
    /// children of `__anw-path`.
    pub recursive: bool,
    pub path_prefix: Option<String>,
    pub limit: Option<usize>,
}

impl ParsedQuery {
    fn new() -> Self {
        Self {
            predicates: Vec::new(),
            recursive: true,
            path_prefix: None,
            limit: None,
        }
    }
}

/// Parse the raw query string (`a=b&c__in=x,y&__anw-limit=5`) into a
/// [`ParsedQuery`]. Returns `QueryError` for unknown operators, malformed
/// control parameters, or unparseable bool/int values.
///
/// # Errors
/// - [`QueryError::UnknownOperator`] for unrecognized `__` suffixes;
/// - [`QueryError::BadControl`] for unrecognized or unparseable
///   `__anw-...` parameters;
/// - [`QueryError::BadValue`] for percent-encoding that fails to decode.
pub fn parse(raw: &str) -> Result<ParsedQuery, QueryError> {
    let mut q = ParsedQuery::new();
    if raw.is_empty() {
        return Ok(q);
    }
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let key = percent_decode(k).ok_or_else(|| QueryError::BadValue(k.to_string()))?;
        let value = percent_decode(v).ok_or_else(|| QueryError::BadValue(v.to_string()))?;

        if let Some(name) = key.strip_prefix("__anw-") {
            apply_control(&mut q, name, &value)?;
            continue;
        }

        let (field, op) = split_operator(&key)?;
        // Pre-validate values whose error-shape is documented per ANW-15 sie
        // review #4 (regex) and #5 (exists). Surface 400s at parse time
        // instead of silently zero-matching.
        match op {
            Operator::Regex => {
                Regex::new(&value)
                    .map_err(|e| QueryError::BadValue(format!("{key}: invalid regex: {e}")))?;
            }
            Operator::Exists => {
                value
                    .parse::<bool>()
                    .map_err(|_| QueryError::BadValue(format!("{key}={value}")))?;
            }
            _ => {}
        }
        q.predicates.push(Predicate {
            field: field.to_string(),
            op,
            value,
        });
    }
    Ok(q)
}

fn split_operator(key: &str) -> Result<(&str, Operator), QueryError> {
    if let Some(idx) = key.rfind("__") {
        let suffix = &key[idx + 2..];
        let op = match suffix {
            "in" => Operator::In,
            "all" => Operator::All,
            "not" => Operator::Not,
            "exists" => Operator::Exists,
            "regex" => Operator::Regex,
            "prefix" => Operator::Prefix,
            "gt" => Operator::Gt,
            "gte" => Operator::Gte,
            "lt" => Operator::Lt,
            "lte" => Operator::Lte,
            // Reject unknown suffixes (e.g. `__contians`) rather than
            // silently treating them as part of the field name -- the User
            // Manual mandates 400 here.
            unknown => return Err(QueryError::UnknownOperator(unknown.to_string())),
        };
        Ok((&key[..idx], op))
    } else {
        Ok((key, Operator::Eq))
    }
}

fn apply_control(q: &mut ParsedQuery, name: &str, value: &str) -> Result<(), QueryError> {
    match name {
        "recursive" => {
            q.recursive = value
                .parse::<bool>()
                .map_err(|_| QueryError::BadControl(format!("recursive={value}")))?;
        }
        "path" => {
            q.path_prefix = Some(value.trim_start_matches('/').to_string());
        }
        "limit" => {
            let n: usize = value
                .parse()
                .map_err(|_| QueryError::BadControl(format!("limit={value}")))?;
            q.limit = Some(n);
        }
        unknown => return Err(QueryError::BadControl(unknown.to_string())),
    }
    Ok(())
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hi = hex_value(bytes[i + 1])?;
                let lo = hex_value(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// One result row in the `/query` response. Per User Manual the body is
/// elided -- consumers fetch bodies via `/notes/<path>` if needed.
#[derive(Debug, Serialize)]
pub struct ResultEntry {
    pub path: String,
    pub frontmatter: JsonValue,
    pub last_modified: String,
    pub etag: String,
    pub size: u64,
}

#[derive(Debug, Serialize)]
pub struct QueryResponse {
    pub results: Vec<ResultEntry>,
    pub total: usize,
    pub truncated: bool,
}

/// Run a parsed query against the in-memory note set.
///
/// Per ANW-15 sie review #6, the per-match clone shed inside the
/// read-lock closure projects directly into the response shape -- no
/// `raw_bytes` / `body` copy happens for `/query`.
pub fn execute<F>(store: &NoteStore, query: &ParsedQuery, format_ts: F) -> QueryResponse
where
    F: Fn(chrono::DateTime<chrono::Utc>) -> String,
{
    let prefix = query.path_prefix.as_deref().unwrap_or("");
    let matches = store.with_read(|notes| {
        let mut out: Vec<ResultEntry> = Vec::new();
        for (path, note) in notes {
            if !path_matches(path, prefix, query.recursive) {
                continue;
            }
            if !predicates_match(&query.predicates, &note.frontmatter) {
                continue;
            }
            out.push(ResultEntry {
                path: note.path.clone(),
                frontmatter: frontmatter_to_json(&note.frontmatter),
                last_modified: format_ts(note.last_modified),
                etag: note.etag.clone(),
                size: note.size,
            });
        }
        out
    });
    let total = matches.len();
    let (results, truncated): (Vec<ResultEntry>, bool) = match query.limit {
        Some(limit) if matches.len() > limit => (matches.into_iter().take(limit).collect(), true),
        _ => (matches, false),
    };
    QueryResponse {
        results,
        total,
        truncated,
    }
}

fn path_matches(path: &str, prefix: &str, recursive: bool) -> bool {
    if prefix.is_empty() {
        return recursive || !path.contains('/');
    }
    // Anchor the prefix on a segment boundary so `__anw-path=Projects`
    // doesn't match `Projects-old/x.md`. Fix-pattern matches the folder
    // listing in ANW-14 (sie's ANW-15 review #3).
    let scoped = if path == prefix {
        // The prefix itself names a note; consider it in-scope.
        ""
    } else if let Some(rest) = path.strip_prefix(&format!("{prefix}/")) {
        rest
    } else {
        return false;
    };
    if recursive || scoped.is_empty() {
        true
    } else {
        !scoped.contains('/')
    }
}

fn predicates_match(predicates: &[Predicate], fm: &Frontmatter) -> bool {
    predicates.iter().all(|p| predicate_match(p, fm))
}

fn predicate_match(p: &Predicate, fm: &Frontmatter) -> bool {
    let target = lookup_dotted(fm, &p.field);
    match p.op {
        Operator::Exists => {
            let want = p.value == "true";
            target.is_some() == want
        }
        Operator::Eq => target.is_some_and(|v| matches_eq(v, &p.value)),
        Operator::Not => target.is_none_or(|v| !matches_eq(v, &p.value)),
        Operator::In => {
            let needles = comma_list(&p.value);
            target.is_some_and(|v| needles.iter().any(|n| matches_eq(v, n)))
        }
        Operator::All => {
            let needles = comma_list(&p.value);
            // __all requires the value to be a list containing every needle.
            let Some(Value::Sequence(seq)) = target else {
                return false;
            };
            needles
                .iter()
                .all(|n| seq.iter().any(|item| matches_scalar_eq(item, n)))
        }
        Operator::Regex => match target {
            Some(Value::String(s)) => Regex::new(&p.value).is_ok_and(|re| re.is_match(s)),
            _ => false,
        },
        Operator::Prefix => match target {
            Some(Value::String(s)) => s.starts_with(&p.value),
            _ => false,
        },
        Operator::Gt | Operator::Gte | Operator::Lt | Operator::Lte => {
            target.is_some_and(|v| ordered_compare(v, &p.value, p.op))
        }
    }
}

fn comma_list(s: &str) -> Vec<String> {
    s.split(',').map(|t| t.trim().to_string()).collect()
}

/// True if `v` equals `needle` -- with the User Manual's contract for
/// scalar-list distinction:
///
/// - if `v` is a scalar, compare value-as-string;
/// - if `v` is a list, *array contains* `needle`.
///
/// `__in` operator handles list-OR semantics separately.
fn matches_eq(v: &Value, needle: &str) -> bool {
    match v {
        Value::Sequence(seq) => seq.iter().any(|item| matches_scalar_eq(item, needle)),
        scalar => matches_scalar_eq(scalar, needle),
    }
}

fn matches_scalar_eq(v: &Value, needle: &str) -> bool {
    match v {
        Value::Null => needle == "null",
        Value::Bool(b) => needle == b.to_string(),
        Value::Int(i) => needle == i.to_string(),
        Value::Float(f) => needle == f.to_string(),
        Value::String(s) => s == needle,
        Value::Date(d) => d.format("%Y-%m-%d").to_string() == needle,
        Value::DateTime(dt) => {
            // Canonical form per the index contract.
            use chrono::SecondsFormat;
            let canon = dt
                .with_timezone(&chrono::Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true);
            canon == needle
        }
        Value::Sequence(_) | Value::Mapping(_) => false,
    }
}

fn ordered_compare(v: &Value, needle: &str, op: Operator) -> bool {
    // Try date -> datetime -> int -> float -> string in turn. Stop at the
    // first parser that matches both sides.
    if let (Some(a), Some(b)) = (
        try_as_date(v),
        NaiveDate::parse_from_str(needle, "%Y-%m-%d").ok(),
    ) {
        return ord(a.cmp(&b), op);
    }
    if let (Some(a), Some(b)) = (
        try_as_datetime(v),
        DateTime::parse_from_rfc3339(needle).ok(),
    ) {
        return ord(a.cmp(&b.with_timezone(&chrono::Utc)), op);
    }
    if let (Some(a), Some(b)) = (try_as_int(v), needle.parse::<i64>().ok()) {
        return ord(a.cmp(&b), op);
    }
    if let (Some(a), Some(b)) = (try_as_float(v), needle.parse::<f64>().ok()) {
        return ord(a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal), op);
    }
    if let Some(a) = try_as_string(v) {
        return ord(a.as_str().cmp(needle), op);
    }
    false
}

fn ord(c: std::cmp::Ordering, op: Operator) -> bool {
    use std::cmp::Ordering;
    match op {
        Operator::Gt => c == Ordering::Greater,
        Operator::Gte => c != Ordering::Less,
        Operator::Lt => c == Ordering::Less,
        Operator::Lte => c != Ordering::Greater,
        _ => false,
    }
}

fn try_as_date(v: &Value) -> Option<NaiveDate> {
    match v {
        Value::Date(d) => Some(*d),
        _ => None,
    }
}

fn try_as_datetime(v: &Value) -> Option<DateTime<chrono::Utc>> {
    match v {
        Value::DateTime(dt) => Some(dt.with_timezone(&chrono::Utc)),
        _ => None,
    }
}

fn try_as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        _ => None,
    }
}

fn try_as_float(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f),
        // i64 -> f64 loses precision above 2^53 -- the User Manual's
        // numeric operators are documented for "typed dates and integers",
        // and a frontmatter integer wider than 2^53 is not a v1 concern.
        #[allow(clippy::cast_precision_loss)]
        Value::Int(i) => Some(*i as f64),
        _ => None,
    }
}

fn try_as_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// Traverse a dotted key path (`author.name`) into the frontmatter tree.
/// Returns `None` if any intermediate key is missing or addresses a non-map.
fn lookup_dotted<'a>(fm: &'a Frontmatter, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let head = parts.next()?;
    let mut current: &Value = fm.get(head)?;
    for part in parts {
        let Value::Mapping(map) = current else {
            return None;
        };
        current = map.get(part)?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use std::collections::BTreeMap;

    use crate::vault::Note;

    fn note(path: &str, fm: Frontmatter) -> Note {
        Note {
            path: path.into(),
            frontmatter: fm,
            body: String::new(),
            raw_bytes: Vec::new(),
            last_modified: Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap(),
            etag: "\"x\"".into(),
            size: 0,
        }
    }

    fn fm(pairs: &[(&str, Value)]) -> Frontmatter {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn store_with(notes: Vec<Note>) -> std::sync::Arc<NoteStore> {
        let s = NoteStore::new();
        s.replace(notes);
        s
    }

    fn run(q: &ParsedQuery, s: &NoteStore) -> QueryResponse {
        execute(s, q, |_dt| String::new())
    }

    #[test]
    fn unknown_operator_returns_error() {
        let err = parse("title__contians=foo").unwrap_err();
        assert!(matches!(err, QueryError::UnknownOperator(_)));
    }

    #[test]
    fn empty_query_string_returns_default_parse() {
        let q = parse("").unwrap();
        assert!(q.predicates.is_empty());
        assert!(q.recursive);
        assert!(q.path_prefix.is_none());
        assert!(q.limit.is_none());
    }

    #[test]
    fn eq_on_scalar_and_list() {
        let q = parse("tags=python").unwrap();
        let s = store_with(vec![
            note("a.md", fm(&[("tags", Value::String("python".into()))])),
            note(
                "b.md",
                fm(&[(
                    "tags",
                    Value::Sequence(vec![
                        Value::String("python".into()),
                        Value::String("go".into()),
                    ]),
                )]),
            ),
            note("c.md", fm(&[("tags", Value::String("go".into()))])),
        ]);
        let r = run(&q, &s);
        assert_eq!(r.total, 2);
        let paths: Vec<&str> = r.results.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(paths, vec!["a.md", "b.md"]);
    }

    #[test]
    fn in_operator_matches_any() {
        let q = parse("tags__in=python,go").unwrap();
        let s = store_with(vec![
            note("a.md", fm(&[("tags", Value::String("python".into()))])),
            note("b.md", fm(&[("tags", Value::String("rust".into()))])),
            note("c.md", fm(&[("tags", Value::String("go".into()))])),
        ]);
        let r = run(&q, &s);
        assert_eq!(r.total, 2);
    }

    #[test]
    fn all_operator_requires_every_value() {
        let q = parse("tags__all=python,fastapi").unwrap();
        let s = store_with(vec![
            note(
                "ok.md",
                fm(&[(
                    "tags",
                    Value::Sequence(vec![
                        Value::String("python".into()),
                        Value::String("fastapi".into()),
                        Value::String("web".into()),
                    ]),
                )]),
            ),
            note(
                "miss.md",
                fm(&[(
                    "tags",
                    Value::Sequence(vec![Value::String("python".into())]),
                )]),
            ),
        ]);
        let r = run(&q, &s);
        assert_eq!(r.total, 1);
        assert_eq!(r.results[0].path, "ok.md");
    }

    #[test]
    fn not_operator_excludes_match() {
        let q = parse("status__not=draft").unwrap();
        let s = store_with(vec![
            note("d.md", fm(&[("status", Value::String("draft".into()))])),
            note("p.md", fm(&[("status", Value::String("published".into()))])),
            note("u.md", fm(&[])),
        ]);
        let r = run(&q, &s);
        let paths: Vec<&str> = r.results.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(paths, vec!["p.md", "u.md"]); // missing field is "not draft"
    }

    #[test]
    fn exists_operator_true_and_false() {
        let q_true = parse("deprecated__exists=true").unwrap();
        let q_false = parse("deprecated__exists=false").unwrap();
        let s = store_with(vec![
            note("a.md", fm(&[("deprecated", Value::Bool(true))])),
            note("b.md", fm(&[])),
        ]);
        assert_eq!(run(&q_true, &s).total, 1);
        assert_eq!(run(&q_false, &s).total, 1);
    }

    #[test]
    fn regex_operator_on_scalar() {
        let q = parse("title__regex=%5EPDR-%5Cd%2B").unwrap();
        let s = store_with(vec![
            note("a.md", fm(&[("title", Value::String("PDR-007".into()))])),
            note("b.md", fm(&[("title", Value::String("ADR-001".into()))])),
        ]);
        assert_eq!(run(&q, &s).total, 1);
    }

    #[test]
    fn prefix_operator_on_scalar() {
        let q = parse("title__prefix=PDR-").unwrap();
        let s = store_with(vec![
            note("a.md", fm(&[("title", Value::String("PDR-007".into()))])),
            note("b.md", fm(&[("title", Value::String("ADR-001".into()))])),
        ]);
        assert_eq!(run(&q, &s).total, 1);
    }

    #[test]
    fn range_operators_on_dates() {
        let q = parse("date__gte=2026-05-01").unwrap();
        let s = store_with(vec![
            note(
                "old.md",
                fm(&[(
                    "date",
                    Value::Date(NaiveDate::from_ymd_opt(2026, 4, 1).unwrap()),
                )]),
            ),
            note(
                "new.md",
                fm(&[(
                    "date",
                    Value::Date(NaiveDate::from_ymd_opt(2026, 6, 1).unwrap()),
                )]),
            ),
        ]);
        let r = run(&q, &s);
        assert_eq!(r.total, 1);
        assert_eq!(r.results[0].path, "new.md");
    }

    #[test]
    fn control_param_limit_truncates_and_sets_total() {
        let q = parse("__anw-limit=2").unwrap();
        let s = store_with(vec![
            note("a.md", fm(&[])),
            note("b.md", fm(&[])),
            note("c.md", fm(&[])),
            note("d.md", fm(&[])),
        ]);
        let r = run(&q, &s);
        assert_eq!(r.total, 4);
        assert_eq!(r.results.len(), 2);
        assert!(r.truncated);
    }

    #[test]
    fn control_param_path_filters_prefix() {
        let q = parse("__anw-path=Projects/anwesen").unwrap();
        let s = store_with(vec![
            note("Projects/anwesen/x.md", fm(&[])),
            note("Projects/other/y.md", fm(&[])),
            note("Top.md", fm(&[])),
        ]);
        let r = run(&q, &s);
        assert_eq!(r.total, 1);
        assert_eq!(r.results[0].path, "Projects/anwesen/x.md");
    }

    #[test]
    fn control_param_path_anchors_on_segment_boundary() {
        // ANW-22 regression: `Projects` must not match `Projects-old/x.md`.
        let q = parse("__anw-path=Projects").unwrap();
        let s = store_with(vec![
            note("Projects/y.md", fm(&[])),
            note("Projects-old/x.md", fm(&[])),
        ]);
        let r = run(&q, &s);
        assert_eq!(r.total, 1);
        assert_eq!(r.results[0].path, "Projects/y.md");
    }

    #[test]
    fn regex_invalid_returns_bad_value() {
        // ANW-15 sie review #4: invalid regex is 400 at parse time, not
        // silently zero-match at exec time.
        let err = parse("title__regex=(unclosed").unwrap_err();
        assert!(matches!(err, QueryError::BadValue(_)));
    }

    #[test]
    fn exists_unparseable_bool_returns_bad_value() {
        // ANW-15 sie review #5: malformed bool is 400, not silent false.
        let err = parse("deprecated__exists=maybe").unwrap_err();
        assert!(matches!(err, QueryError::BadValue(_)));
    }

    #[test]
    fn control_param_recursive_false_limits_to_direct_children() {
        let q = parse("__anw-path=Projects&__anw-recursive=false").unwrap();
        let s = store_with(vec![
            note("Projects/a.md", fm(&[])),
            note("Projects/sub/b.md", fm(&[])),
        ]);
        let r = run(&q, &s);
        assert_eq!(r.total, 1);
        assert_eq!(r.results[0].path, "Projects/a.md");
    }

    #[test]
    fn nested_dotted_keys() {
        let mut author: BTreeMap<String, Value> = BTreeMap::new();
        author.insert("name".into(), Value::String("brn".into()));
        let q = parse("author.name=brn").unwrap();
        let s = store_with(vec![
            note("a.md", fm(&[("author", Value::Mapping(author))])),
            note("b.md", fm(&[("author", Value::String("someone".into()))])),
        ]);
        assert_eq!(run(&q, &s).total, 1);
    }

    #[test]
    fn comma_escape_via_percent_encoding() {
        // Escaping a literal comma -- ensures __in doesn't split inside an
        // intentional value.
        let q = parse("kind__in=a%2Cb,c").unwrap();
        assert_eq!(q.predicates[0].value, "a,b,c");
    }

    #[test]
    fn unknown_control_param_returns_error() {
        let err = parse("__anw-flarble=1").unwrap_err();
        assert!(matches!(err, QueryError::BadControl(_)));
    }
}
