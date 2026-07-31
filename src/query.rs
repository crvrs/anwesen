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
//! applying each [`Predicate`] in turn. At the documented scale
//! (low-thousands-of-notes vaults) this is sub-millisecond; see
//! [[ADR-009 Reverse ADR-002 In-Memory Evaluation No Tantivy]] for the
//! call to keep evaluation in-memory rather than carrying a Tantivy index.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
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
    /// `__anw-order` -- fragment ordering for markdown-merge mode ([ANW-26]).
    /// `None` orders by path. Meaningful only in merge mode; the JSON path
    /// ignores it.
    pub order: Option<OrderSpec>,
    /// `__anw-kind` -- homogeneity-guard key for markdown-merge mode
    /// ([ANW-26]). Meaningful only in merge mode.
    pub kind_key: Option<String>,
}

/// Parsed `__anw-order=<frontmatter-key>[:asc|:desc]`. `desc` is `false`
/// (ascending) unless the value carries an explicit `:desc` suffix.
#[derive(Debug, Clone)]
pub struct OrderSpec {
    pub key: String,
    pub desc: bool,
}

impl ParsedQuery {
    fn new() -> Self {
        Self {
            predicates: Vec::new(),
            recursive: true,
            path_prefix: None,
            limit: None,
            order: None,
            kind_key: None,
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
        "order" => {
            q.order = Some(parse_order(value)?);
        }
        "kind" => {
            if value.is_empty() {
                return Err(QueryError::BadControl("kind: empty key".to_string()));
            }
            q.kind_key = Some(value.to_string());
        }
        unknown => return Err(QueryError::BadControl(unknown.to_string())),
    }
    Ok(())
}

/// Split `__anw-order` into its key and direction. Only the literal `:asc`
/// and `:desc` suffixes are recognized; any other value is taken whole as the
/// key. An empty key is `400`.
fn parse_order(value: &str) -> Result<OrderSpec, QueryError> {
    let (key, desc) = if let Some(k) = value.strip_suffix(":desc") {
        (k, true)
    } else if let Some(k) = value.strip_suffix(":asc") {
        (k, false)
    } else {
        (value, false)
    };
    if key.is_empty() {
        return Err(QueryError::BadControl(format!("order={value}")));
    }
    Ok(OrderSpec {
        key: key.to_string(),
        desc,
    })
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

/// Canonical RFC 3339 form with a `Z` suffix -- the shape the User Manual
/// example uses for `last_modified`. It lives next to [`ResultEntry`] because
/// it is the projection's dialect: both the HTTP handlers and the offline
/// `anwesen query` subcommand ([ANW-43]) format timestamps with it, so the
/// two surfaces cannot drift.
#[must_use]
pub fn rfc3339_z(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Secs, true)
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

/// A single matched note projected for markdown-merge mode ([ANW-26]).
/// Unlike the JSON path, this deliberately clones the note `body` -- merge
/// mode materializes bodies. The order/kind keys are pulled out under the
/// read lock so the lock is not held during sort/assembly.
struct Fragment {
    path: String,
    body: String,
    order_value: Option<Value>,
    kind_value: Option<Value>,
}

/// Failure modes of [`execute_merge`].
#[derive(Debug, PartialEq, Eq)]
pub enum MergeError {
    /// The `__anw-kind` homogeneity guard rejected the matched set. The
    /// `String` is the ready-to-send `400` body naming the offenders.
    KindGuard(String),
}

/// Run a parsed query in markdown-merge mode: concatenate the bodies of all
/// matched notes into one document, each fragment preceded by an HTML-comment
/// source marker. See [ANW-26].
///
/// `__anw-kind` and `__anw-order` both apply to the full matched set, before
/// `__anw-limit` truncates: the guard's pass/fail is independent of the cap,
/// and the cap keeps the top-N by the order key.
///
/// Returns the assembled document, or [`MergeError::KindGuard`] when the
/// homogeneity guard fails. An empty match set yields an empty document.
///
/// # Errors
/// [`MergeError::KindGuard`] when `__anw-kind` is set and the matched notes
/// do not all carry one and the same value for that key.
pub fn execute_merge(store: &NoteStore, query: &ParsedQuery) -> Result<String, MergeError> {
    let prefix = query.path_prefix.as_deref().unwrap_or("");
    let mut frags = store.with_read(|notes| {
        let mut out: Vec<Fragment> = Vec::new();
        for (path, note) in notes {
            if !path_matches(path, prefix, query.recursive) {
                continue;
            }
            if !predicates_match(&query.predicates, &note.frontmatter) {
                continue;
            }
            let order_value = query
                .order
                .as_ref()
                .and_then(|o| lookup_dotted(&note.frontmatter, &o.key).cloned());
            let kind_value = query
                .kind_key
                .as_ref()
                .and_then(|k| lookup_dotted(&note.frontmatter, k).cloned());
            out.push(Fragment {
                path: note.path.clone(),
                body: note.body.clone(),
                order_value,
                kind_value,
            });
        }
        out
    });

    // Homogeneity guard over the full matched set, before __anw-limit.
    if let Some(kind_key) = &query.kind_key {
        let mut missing: Vec<&str> = Vec::new();
        let mut by_value: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for f in &frags {
            match &f.kind_value {
                None => missing.push(&f.path),
                Some(v) => by_value
                    .entry(canonical_string(v))
                    .or_default()
                    .push(&f.path),
            }
        }
        if !missing.is_empty() || by_value.len() > 1 {
            return Err(MergeError::KindGuard(format_kind_error(
                kind_key, &missing, &by_value,
            )));
        }
    }

    // Order before __anw-limit so the cap keeps the top-N by the order key,
    // not a path-ordered prefix. Tie-break by path (ascending) for byte
    // stability regardless of direction.
    match &query.order {
        Some(spec) => frags.sort_by(|a, b| {
            order_cmp(a.order_value.as_ref(), b.order_value.as_ref(), spec.desc)
                .then_with(|| a.path.cmp(&b.path))
        }),
        None => frags.sort_by(|a, b| a.path.cmp(&b.path)),
    }

    if let Some(limit) = query.limit {
        frags.truncate(limit);
    }

    Ok(assemble(&frags))
}

/// Concatenate fragments into one markdown document. Each fragment is its
/// source marker line followed by the verbatim, frontmatter-stripped body;
/// fragments are joined with a blank line so the result stays valid markdown.
fn assemble(frags: &[Fragment]) -> String {
    frags
        .iter()
        .map(|f| format!("<!-- source: {} -->\n{}", f.path, f.body))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Build the `400` body for a failed `__anw-kind` guard. Distinct values are
/// listed in sorted order, each with its paths; notes missing the key are
/// named separately. Deterministic for byte-stable error responses.
fn format_kind_error(
    key: &str,
    missing: &[&str],
    by_value: &BTreeMap<String, Vec<&str>>,
) -> String {
    use std::fmt::Write as _;
    let mut s = format!(
        "__anw-kind={key}: merge requires every matched note to share one value for `{key}`\n"
    );
    if by_value.len() > 1 {
        s.push_str("distinct values found:\n");
        for (val, paths) in by_value {
            let _ = writeln!(s, "  {val}: {}", paths.join(", "));
        }
    }
    if !missing.is_empty() {
        let _ = writeln!(s, "notes missing the key: {}", missing.join(", "));
    }
    s
}

/// Order two optional frontmatter values for `__anw-order`. Present values
/// sort before missing ones regardless of direction; `desc` reverses only the
/// comparison among present values.
fn order_cmp(a: Option<&Value>, b: Option<&Value>, desc: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Some(x), Some(y)) => {
            let base = compare_values(x, y);
            if desc { base.reverse() } else { base }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Compare two frontmatter values using the same coercion ladder as the
/// range operators (date -> datetime -> int -> float -> string); see
/// [`ordered_compare`]. Falls back to canonical-string comparison when no
/// rung matches both sides.
fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if let (Some(x), Some(y)) = (try_as_date(a), try_as_date(b)) {
        return x.cmp(&y);
    }
    if let (Some(x), Some(y)) = (try_as_datetime(a), try_as_datetime(b)) {
        return x.cmp(&y);
    }
    if let (Some(x), Some(y)) = (try_as_int(a), try_as_int(b)) {
        return x.cmp(&y);
    }
    if let (Some(x), Some(y)) = (try_as_float(a), try_as_float(b)) {
        return x.partial_cmp(&y).unwrap_or(Ordering::Equal);
    }
    canonical_string(a).cmp(&canonical_string(b))
}

/// Render a value to its canonical string -- the same dialect
/// [`matches_scalar_eq`] compares against. Used as the grouping key for the
/// kind guard and as the string-rung fallback for ordering.
fn canonical_string(v: &Value) -> String {
    use chrono::SecondsFormat;
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::String(s) => s.clone(),
        Value::Date(d) => d.format("%Y-%m-%d").to_string(),
        Value::DateTime(dt) => dt
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(SecondsFormat::Secs, true),
        Value::Sequence(_) | Value::Mapping(_) => format!("{v:?}"),
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

    // --- [ANW-26] markdown-merge mode ---

    fn note_body(path: &str, fm: Frontmatter, body: &str) -> Note {
        Note {
            path: path.into(),
            frontmatter: fm,
            body: body.into(),
            raw_bytes: Vec::new(),
            last_modified: Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap(),
            etag: "\"x\"".into(),
            size: 0,
        }
    }

    fn merge(q: &ParsedQuery, s: &NoteStore) -> Result<String, MergeError> {
        execute_merge(s, q)
    }

    #[test]
    fn order_parses_direction_suffix() {
        assert!(!parse("__anw-order=num").unwrap().order.unwrap().desc);
        let asc = parse("__anw-order=num:asc").unwrap().order.unwrap();
        assert_eq!(asc.key, "num");
        assert!(!asc.desc);
        let desc = parse("__anw-order=num:desc").unwrap().order.unwrap();
        assert_eq!(desc.key, "num");
        assert!(desc.desc);
    }

    #[test]
    fn order_empty_key_is_bad_control() {
        assert!(matches!(
            parse("__anw-order=:desc").unwrap_err(),
            QueryError::BadControl(_)
        ));
        assert!(matches!(
            parse("__anw-kind=").unwrap_err(),
            QueryError::BadControl(_)
        ));
    }

    #[test]
    fn merge_concatenates_bodies_with_source_markers() {
        let q = parse("").unwrap();
        let s = store_with(vec![
            note_body("a.md", fm(&[]), "Body A."),
            note_body("b.md", fm(&[]), "Body B."),
        ]);
        let out = merge(&q, &s).unwrap();
        assert_eq!(
            out,
            "<!-- source: a.md -->\nBody A.\n\n<!-- source: b.md -->\nBody B."
        );
    }

    #[test]
    fn merge_empty_match_set_is_empty_document() {
        let q = parse("status=nope").unwrap();
        let s = store_with(vec![note_body("a.md", fm(&[]), "Body A.")]);
        assert_eq!(merge(&q, &s).unwrap(), "");
    }

    #[test]
    fn merge_order_applies_before_limit() {
        // Three notes ordered by `num` descending, capped to 2 -> keeps the
        // top-2 by num (3, 2), not a path-ordered prefix.
        let q = parse("__anw-order=num:desc&__anw-limit=2").unwrap();
        let s = store_with(vec![
            note_body("a.md", fm(&[("num", Value::Int(1))]), "one"),
            note_body("b.md", fm(&[("num", Value::Int(3))]), "three"),
            note_body("c.md", fm(&[("num", Value::Int(2))]), "two"),
        ]);
        let out = merge(&q, &s).unwrap();
        assert_eq!(
            out,
            "<!-- source: b.md -->\nthree\n\n<!-- source: c.md -->\ntwo"
        );
    }

    #[test]
    fn merge_order_missing_key_sorts_last_then_by_path() {
        let q = parse("__anw-order=num").unwrap();
        let s = store_with(vec![
            note_body("z.md", fm(&[("num", Value::Int(5))]), "five"),
            note_body("m.md", fm(&[]), "no-num-m"),
            note_body("a.md", fm(&[]), "no-num-a"),
        ]);
        let out = merge(&q, &s).unwrap();
        // num=5 first; the two key-less notes follow, tie-broken by path.
        assert_eq!(
            out,
            "<!-- source: z.md -->\nfive\n\n<!-- source: a.md -->\nno-num-a\n\n<!-- source: m.md -->\nno-num-m"
        );
    }

    #[test]
    fn merge_kind_guard_passes_when_uniform() {
        let q = parse("__anw-kind=kind").unwrap();
        let s = store_with(vec![
            note_body("a.md", fm(&[("kind", Value::String("PDR".into()))]), "a"),
            note_body("b.md", fm(&[("kind", Value::String("PDR".into()))]), "b"),
        ]);
        assert!(merge(&q, &s).is_ok());
    }

    #[test]
    fn merge_kind_guard_rejects_distinct_values() {
        let q = parse("__anw-kind=kind").unwrap();
        let s = store_with(vec![
            note_body("a.md", fm(&[("kind", Value::String("PDR".into()))]), "a"),
            note_body("b.md", fm(&[("kind", Value::String("ADR".into()))]), "b"),
        ]);
        let err = merge(&q, &s).unwrap_err();
        let MergeError::KindGuard(msg) = err;
        assert!(msg.contains("ADR"));
        assert!(msg.contains("PDR"));
        assert!(msg.contains("a.md"));
        assert!(msg.contains("b.md"));
    }

    #[test]
    fn merge_kind_guard_rejects_missing_key() {
        let q = parse("__anw-kind=kind").unwrap();
        let s = store_with(vec![
            note_body("a.md", fm(&[("kind", Value::String("PDR".into()))]), "a"),
            note_body("b.md", fm(&[]), "b"),
        ]);
        let err = merge(&q, &s).unwrap_err();
        let MergeError::KindGuard(msg) = err;
        assert!(msg.contains("missing the key"));
        assert!(msg.contains("b.md"));
    }

    #[test]
    fn merge_kind_guard_evaluated_over_full_set_before_limit() {
        // The limit would keep only the first note (uniform), but the guard
        // must still see the mismatched second note and reject.
        let q = parse("__anw-kind=kind&__anw-limit=1&__anw-order=num").unwrap();
        let s = store_with(vec![
            note_body(
                "a.md",
                fm(&[
                    ("kind", Value::String("PDR".into())),
                    ("num", Value::Int(1)),
                ]),
                "a",
            ),
            note_body(
                "b.md",
                fm(&[
                    ("kind", Value::String("ADR".into())),
                    ("num", Value::Int(2)),
                ]),
                "b",
            ),
        ]);
        assert!(merge(&q, &s).is_err());
    }
}
