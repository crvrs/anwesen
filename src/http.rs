//! HTTP surface for Anwesen.
//!
//! Implements the read-one-note endpoint per [ANW-13]:
//!
//! ```text
//! GET /notes/<path>          -> JSON {path, frontmatter, body, last_modified, etag, size}
//! GET /notes/<path>          + Accept: text/markdown -> raw file bytes
//! GET /notes/<path>          + If-None-Match: "<etag>" -> 304 on match
//! ```
//!
//! Subsequent issues bolt the folder index ([ANW-14]), query ([ANW-15]) and
//! `/health` ([ANW-8]) onto the same [`Router`].

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{OriginalUri, Path as AxumPath, Request, State};
use axum::http::header::{ACCEPT, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{Next, from_fn};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, SecondsFormat, Utc};
use hydra::Process;
use serde::Serialize;
use std::collections::BTreeMap;

use std::path::PathBuf;

use crate::app::RestartCounters;
use crate::health::HealthState;
use crate::store::NoteStore;
use crate::vault::{Note, frontmatter_to_json};

/// Canonical RFC 3339 form with a `Z` suffix -- the shape the User Manual
/// example uses for `last_modified`. Centralized here so every HTTP
/// `last_modified` field stays in the same dialect.
fn rfc3339_z(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Shared state injected into every handler.
#[derive(Clone)]
pub struct HttpState {
    pub store: Arc<NoteStore>,
    pub health: Arc<HealthState>,
    pub restart_counters: Arc<RestartCounters>,
    pub vault: PathBuf,
}

pub fn router(state: HttpState) -> Router {
    // `/notes/{*path}` is greedy and includes any trailing slash; one
    // handler dispatches read-one vs folder-listing on that suffix. The
    // root listing (`/notes/`) needs its own route since the wildcard
    // requires at least one character.
    Router::new()
        .route("/notes/", get(list_root_folder))
        .route("/notes/{*path}", get(get_notes))
        .route("/query", get(get_query))
        .route("/health", get(get_health))
        .layer(from_fn(process_wrap))
        .with_state(state)
}

/// Per [ANW-25](https://crvrs.youtrack.cloud/issue/ANW-25): run each request
/// handler inside its own Hydra process via [`Process::spawn`], delivering
/// the response back through a `oneshot`. A handler panic or abnormal exit
/// drops the sender, the receiver returns `Err`, and the middleware
/// surfaces a `500` instead of poisoning the `http_server` process.
async fn process_wrap(request: Request, next: Next) -> Response {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    Process::spawn(async move {
        if sender.send(next.run(request).await).is_err() {
            tracing::error!("process_wrap: response receiver dropped before handler completed");
        }
    });
    if let Ok(response) = receiver.await {
        response
    } else {
        tracing::error!("process_wrap: handler process exited before responding");
        (StatusCode::INTERNAL_SERVER_ERROR, "internal server error").into_response()
    }
}

#[derive(Debug, Serialize)]
struct NoteResponse<'a> {
    path: &'a str,
    frontmatter: serde_json::Value,
    body: &'a str,
    last_modified: String,
    etag: &'a str,
    size: u64,
}

impl<'a> From<&'a Note> for NoteResponse<'a> {
    fn from(note: &'a Note) -> Self {
        Self {
            path: &note.path,
            frontmatter: frontmatter_to_json(&note.frontmatter),
            body: &note.body,
            last_modified: rfc3339_z(note.last_modified),
            etag: &note.etag,
            size: note.size,
        }
    }
}

async fn get_notes(
    State(state): State<HttpState>,
    AxumPath(path): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if path.ends_with('/') {
        let folder = &path[..path.len() - 1];
        match resolve_folder(folder) {
            Err(reason) => bad_request(reason).into_response(),
            Ok(canonical) => list_folder(&state, &canonical).into_response(),
        }
    } else {
        match resolve_path(&path) {
            Err(reason) => bad_request(reason).into_response(),
            Ok(canonical) => match state.store.get(&canonical) {
                None => not_found().into_response(),
                Some(note) => respond_note(&note, &headers).into_response(),
            },
        }
    }
}

async fn list_root_folder(State(state): State<HttpState>) -> Response {
    list_folder(&state, "").into_response()
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    vault_path: String,
    note_count: usize,
    last_index_update_ts: Option<String>,
    last_event_ts: Option<String>,
    watcher_state: &'static str,
    in_flight_rescan: bool,
    supervisor: SupervisorBlock,
}

#[derive(Debug, Serialize)]
struct SupervisorBlock {
    restarts: BTreeMap<&'static str, u32>,
}

async fn get_health(State(state): State<HttpState>) -> Response {
    let body = HealthResponse {
        vault_path: state.vault.to_string_lossy().into_owned(),
        note_count: state.store.len(),
        last_index_update_ts: state.health.last_index_update().map(rfc3339_z),
        last_event_ts: state.health.last_event().map(rfc3339_z),
        watcher_state: state.health.watcher_state().as_str(),
        in_flight_rescan: state.health.in_flight_rescan(),
        supervisor: SupervisorBlock {
            restarts: state.restart_counters.snapshot(),
        },
    };
    let bytes = serde_json::to_vec(&body).expect("health response serializes");
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .expect("static response")
}

async fn get_query(
    State(state): State<HttpState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let raw = uri.query().unwrap_or("");
    let parsed = match crate::query::parse(raw) {
        Ok(p) => p,
        Err(e) => return bad_request(&e.to_string()).into_response(),
    };

    // `Accept: text/markdown` selects merge mode: concatenated note bodies
    // ([ANW-26]). Without it, `/query` returns the JSON list as before.
    if wants_markdown(&headers) {
        return match crate::query::execute_merge(&state.store, &parsed) {
            Ok(body) => Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, "text/markdown; charset=utf-8")
                .body(Body::from(body))
                .expect("static response"),
            Err(crate::query::MergeError::KindGuard(msg)) => bad_request(&msg).into_response(),
        };
    }

    let resp = crate::query::execute(&state.store, &parsed, rfc3339_z);
    let bytes = serde_json::to_vec(&resp).expect("query response serializes");
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .expect("static response")
}

fn respond_note(note: &Note, headers: &HeaderMap) -> Response {
    if let Some(client_etag) = headers.get(IF_NONE_MATCH)
        && etag_matches(client_etag, &note.etag)
    {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(ETAG, note.etag.as_str())
            .body(Body::empty())
            .expect("static response");
    }

    if wants_markdown(headers) {
        return Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "text/markdown; charset=utf-8")
            .header(ETAG, note.etag.as_str())
            .body(Body::from(note.raw_bytes.clone()))
            .expect("static response");
    }

    let body = serde_json::to_vec(&NoteResponse::from(note)).expect("note serializes");
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .header(ETAG, note.etag.as_str())
        .body(Body::from(body))
        .expect("static response")
}

fn wants_markdown(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .any(|raw| {
            // Strip media-type parameters (`text/markdown; q=0.9`) before compare.
            let mime = raw.split(';').next().unwrap_or(raw).trim();
            mime.eq_ignore_ascii_case("text/markdown")
        })
}

fn etag_matches(client: &HeaderValue, server_etag: &str) -> bool {
    let Ok(s) = client.to_str() else {
        return false;
    };
    // `If-None-Match` may carry one or more comma-separated entity-tags;
    // we ignore weak validators (W/) since the server only emits strong.
    s.split(',').any(|raw| {
        let trimmed = raw.trim();
        let stripped = trimmed.strip_prefix("W/").unwrap_or(trimmed);
        stripped == server_etag || trimmed == "*"
    })
}

fn bad_request(reason: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, format!("bad request: {reason}\n"))
}

fn not_found() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "not found\n")
}

#[derive(Debug, Serialize)]
struct FolderResponse {
    path: String,
    entries: Vec<FolderEntry>,
}

#[derive(Debug, Serialize)]
struct FolderEntry {
    name: String,
    #[serde(rename = "type")]
    kind: &'static str,
    last_modified: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
}

fn list_folder(state: &HttpState, folder: &str) -> Response {
    let prefix = if folder.is_empty() {
        String::new()
    } else {
        format!("{folder}/")
    };

    // Two maps: file_name -> (last_modified, size), dir_name -> max(last_modified).
    let mut files: BTreeMap<String, (DateTime<Utc>, u64)> = BTreeMap::new();
    let mut dirs: BTreeMap<String, DateTime<Utc>> = BTreeMap::new();
    let mut found_any = false;

    state.store.with_read(|notes| {
        for (path, note) in notes {
            let Some(rel) = path.strip_prefix(prefix.as_str()) else {
                continue;
            };
            // Empty rel happens if `path == prefix.trim_end_matches('/')`;
            // BTreeMap iteration with `strip_prefix` won't produce that
            // (the prefix has a trailing '/'), so a non-empty rel is the
            // only shape we see here.
            if rel.is_empty() {
                continue;
            }
            found_any = true;
            match rel.find('/') {
                Some(idx) => {
                    let name = rel[..idx].to_string();
                    dirs.entry(name)
                        .and_modify(|t| {
                            if note.last_modified > *t {
                                *t = note.last_modified;
                            }
                        })
                        .or_insert(note.last_modified);
                }
                None => {
                    // Leaf file. Scanner already filtered to *.md.
                    files.insert(rel.to_string(), (note.last_modified, note.size));
                }
            }
        }
    });

    if !found_any && !folder.is_empty() {
        return not_found().into_response();
    }

    let mut entries: Vec<FolderEntry> = Vec::with_capacity(files.len() + dirs.len());
    for (name, (lm, size)) in files {
        entries.push(FolderEntry {
            name,
            kind: "file",
            last_modified: rfc3339_z(lm),
            size: Some(size),
        });
    }
    for (name, lm) in dirs {
        entries.push(FolderEntry {
            name,
            kind: "dir",
            last_modified: rfc3339_z(lm),
            size: None,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    let body = FolderResponse {
        path: folder.to_string(),
        entries,
    };
    let bytes = serde_json::to_vec(&body).expect("folder response serializes");
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .expect("static response")
}

/// Normalize a request path per the User Manual:
///
/// - URL-decode the entire path once;
/// - strip leading slashes;
/// - reject any `..` segment (or empty segment / `.`) with `400`.
///
/// Returns the vault-relative form (forward-slash, no leading slash).
fn resolve_path(raw: &str) -> Result<String, &'static str> {
    let decoded = percent_decode(raw).ok_or("invalid percent-encoding")?;
    let stripped = decoded.trim_start_matches('/').to_string();
    if stripped.is_empty() {
        return Err("path is empty");
    }
    for seg in stripped.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err("path contains forbidden segment");
        }
    }
    Ok(stripped)
}

/// Same normalization as [`resolve_path`] but accepts the empty string -- a
/// folder listing of the vault root via `/notes/`.
fn resolve_folder(raw: &str) -> Result<String, &'static str> {
    let decoded = percent_decode(raw).ok_or("invalid percent-encoding")?;
    let stripped = decoded.trim_start_matches('/').to_string();
    if stripped.is_empty() {
        return Ok(String::new());
    }
    for seg in stripped.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err("path contains forbidden segment");
        }
    }
    Ok(stripped)
}

/// Tiny dependency-free single-pass percent-decoder. Anwesen URL-decodes
/// each path component exactly once per the User Manual contract; we keep
/// the decoder small rather than pull `percent-encoding` for one call.
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = hex_value(bytes[i + 1])?;
            let lo = hex_value(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use chrono::DateTime;
    use std::collections::BTreeMap;
    use tower::ServiceExt;

    use crate::vault::Value;

    fn note(path: &str, body: &str) -> Note {
        let mut fm: BTreeMap<String, Value> = BTreeMap::new();
        fm.insert("tag".into(), Value::String("demo".into()));
        let raw = format!("---\ntag: demo\n---\n{body}");
        let etag = format!("\"{}\"", blake3::hash(raw.as_bytes()).to_hex());
        let size = raw.len() as u64;
        Note {
            path: path.into(),
            frontmatter: fm,
            body: body.into(),
            raw_bytes: raw.into_bytes(),
            last_modified: DateTime::from_timestamp(0, 0).unwrap(),
            etag,
            size,
        }
    }

    fn router_with(notes: Vec<Note>) -> (Router, Arc<NoteStore>) {
        let store = NoteStore::new();
        store.replace(notes);
        let r = router(HttpState {
            store: store.clone(),
            health: crate::health::HealthState::new(),
            restart_counters: crate::app::RestartCounters::new(),
            vault: PathBuf::from("/test/vault"),
        });
        (r, store)
    }

    async fn send(router: Router, req: Request<Body>) -> Response {
        router.oneshot(req).await.expect("oneshot")
    }

    #[tokio::test]
    async fn unknown_path_returns_404() {
        let (r, _) = router_with(vec![]);
        let req = Request::get("/notes/missing.md")
            .body(Body::empty())
            .unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn double_dot_rejected_with_400() {
        let (r, _) = router_with(vec![note("a.md", "x")]);
        let req = Request::get("/notes/../etc/passwd")
            .body(Body::empty())
            .unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn percent_encoded_path_decoded_once() {
        let (r, _) = router_with(vec![note("dir with space/a.md", "x")]);
        let req = Request::get("/notes/dir%20with%20space/a.md")
            .body(Body::empty())
            .unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn default_response_is_json_with_etag_header() {
        let n = note("a.md", "body");
        let expected_etag = n.etag.clone();
        let (r, _) = router_with(vec![n]);
        let req = Request::get("/notes/a.md").body(Body::empty()).unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(resp.headers().get(ETAG).unwrap(), expected_etag.as_str());
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["path"], "a.md");
        assert_eq!(v["body"], "body");
        assert!(v["frontmatter"].is_object());
    }

    #[tokio::test]
    async fn accept_text_markdown_returns_raw_bytes() {
        let n = note("a.md", "body");
        let raw = n.raw_bytes.clone();
        let (r, _) = router_with(vec![n]);
        let req = Request::get("/notes/a.md")
            .header(ACCEPT, "text/markdown")
            .body(Body::empty())
            .unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            "text/markdown; charset=utf-8"
        );
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        assert_eq!(bytes.as_ref(), raw.as_slice());
    }

    #[tokio::test]
    async fn if_none_match_returns_304() {
        let n = note("a.md", "body");
        let etag = n.etag.clone();
        let (r, _) = router_with(vec![n]);
        let req = Request::get("/notes/a.md")
            .header(IF_NONE_MATCH, etag.clone())
            .body(Body::empty())
            .unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(resp.headers().get(ETAG).unwrap(), etag.as_str());
        // 304 must not carry a body.
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn if_none_match_star_returns_304() {
        let n = note("a.md", "body");
        let (r, _) = router_with(vec![n]);
        let req = Request::get("/notes/a.md")
            .header(IF_NONE_MATCH, "*")
            .body(Body::empty())
            .unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn if_none_match_with_different_etag_returns_full_body() {
        let n = note("a.md", "body");
        let (r, _) = router_with(vec![n]);
        let req = Request::get("/notes/a.md")
            .header(IF_NONE_MATCH, "\"other\"")
            .body(Body::empty())
            .unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn folder_listing_groups_files_and_dirs() {
        let notes = vec![
            note("Projects/a.md", "x"),
            note("Projects/anwesen/Anwesen.md", "x"),
            note("Projects/anwesen/User Manual.md", "x"),
            note("Projects/anwesen/ADR/ADR-001.md", "x"),
            note("Notes/other.md", "x"),
        ];
        let (r, _) = router_with(notes);
        let req = Request::get("/notes/Projects/anwesen/")
            .body(Body::empty())
            .unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["path"], "Projects/anwesen");
        let entries = v["entries"].as_array().expect("entries array");
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["ADR", "Anwesen.md", "User Manual.md"]);
        // ADR is the only dir; the other two are files (lex-sorted).
        assert_eq!(entries[0]["type"], "dir");
        assert!(entries[0].get("size").is_none());
        assert_eq!(entries[1]["type"], "file");
        assert_eq!(entries[1]["size"].as_u64().unwrap(), note("x", "x").size);
    }

    #[tokio::test]
    async fn folder_listing_root_lists_top_level() {
        let notes = vec![
            note("a.md", "x"),
            note("Projects/anwesen/x.md", "x"),
            note("Notes/other.md", "x"),
        ];
        let (r, _) = router_with(notes);
        let req = Request::get("/notes/").body(Body::empty()).unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["path"], "");
        let names: Vec<&str> = v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["Notes", "Projects", "a.md"]);
    }

    #[tokio::test]
    async fn folder_listing_unknown_returns_404() {
        let (r, _) = router_with(vec![note("a.md", "x")]);
        let req = Request::get("/notes/no-such-folder/")
            .body(Body::empty())
            .unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn folder_listing_double_dot_rejected_with_400() {
        let (r, _) = router_with(vec![note("a.md", "x")]);
        let req = Request::get("/notes/Projects/../etc/")
            .body(Body::empty())
            .unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn folder_listing_prefix_does_not_match_partial_segment() {
        // "Proj" is not a folder; only "Projects" is.
        let (r, _) = router_with(vec![note("Projects/a.md", "x"), note("Projet/b.md", "x")]);
        let req = Request::get("/notes/Proj/").body(Body::empty()).unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Note builder with caller-chosen frontmatter and body, for the
    /// [ANW-26] merge-mode tests.
    fn note_fm(path: &str, fm: BTreeMap<String, Value>, body: &str) -> Note {
        let raw = format!("---\n# fm\n---\n{body}");
        let etag = format!("\"{}\"", blake3::hash(raw.as_bytes()).to_hex());
        let size = raw.len() as u64;
        Note {
            path: path.into(),
            frontmatter: fm,
            body: body.into(),
            raw_bytes: raw.into_bytes(),
            last_modified: DateTime::from_timestamp(0, 0).unwrap(),
            etag,
            size,
        }
    }

    fn kind_fm(kind: &str) -> BTreeMap<String, Value> {
        let mut fm = BTreeMap::new();
        fm.insert("kind".into(), Value::String(kind.into()));
        fm
    }

    #[tokio::test]
    async fn query_accept_markdown_merges_bodies() {
        let (r, _) = router_with(vec![note("a.md", "Body A."), note("b.md", "Body B.")]);
        let req = Request::get("/query")
            .header(ACCEPT, "text/markdown")
            .body(Body::empty())
            .unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            "text/markdown; charset=utf-8"
        );
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(
            text,
            "<!-- source: a.md -->\nBody A.\n\n<!-- source: b.md -->\nBody B."
        );
    }

    #[tokio::test]
    async fn query_without_accept_is_json_no_body() {
        let (r, _) = router_with(vec![note("a.md", "Body A.")]);
        let req = Request::get("/query").body(Body::empty()).unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["total"], 1);
        assert!(v["results"][0].get("body").is_none());
    }

    #[tokio::test]
    async fn query_markdown_kind_guard_returns_400() {
        let (r, _) = router_with(vec![
            note_fm("a.md", kind_fm("PDR"), "a"),
            note_fm("b.md", kind_fm("ADR"), "b"),
        ]);
        let req = Request::get("/query?__anw-kind=kind")
            .header(ACCEPT, "text/markdown")
            .body(Body::empty())
            .unwrap();
        let resp = send(r, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("PDR"));
        assert!(text.contains("ADR"));
    }

    #[test]
    fn resolve_path_normalizations() {
        assert_eq!(resolve_path("Notes/a.md").unwrap(), "Notes/a.md");
        assert_eq!(resolve_path("/Notes/a.md").unwrap(), "Notes/a.md");
        assert_eq!(resolve_path("////Notes/a.md").unwrap(), "Notes/a.md");
        assert_eq!(resolve_path("Notes/a%20b.md").unwrap(), "Notes/a b.md");
        assert!(resolve_path("..").is_err());
        assert!(resolve_path("a/../b").is_err());
        assert!(resolve_path("a//b").is_err());
        assert!(resolve_path("a/./b").is_err());
        assert!(resolve_path("").is_err());
        assert!(resolve_path("a/%ZZ.md").is_err());
    }
}
