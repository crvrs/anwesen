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
use axum::extract::{Path as AxumPath, State};
use axum::http::header::{ACCEPT, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;

use crate::store::NoteStore;
use crate::vault::{Note, frontmatter_to_json};

/// Shared state injected into every handler.
#[derive(Clone)]
pub struct HttpState {
    pub store: Arc<NoteStore>,
}

pub fn router(state: HttpState) -> Router {
    Router::new()
        .route("/notes/{*path}", get(get_note))
        .with_state(state)
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
            last_modified: note.last_modified.to_rfc3339(),
            etag: &note.etag,
            size: note.size,
        }
    }
}

async fn get_note(
    State(state): State<HttpState>,
    AxumPath(path): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    match resolve_path(&path) {
        Err(reason) => bad_request(reason).into_response(),
        Ok(canonical) => match state.store.get(&canonical) {
            None => not_found().into_response(),
            Some(note) => respond_note(&note, &headers).into_response(),
        },
    }
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
