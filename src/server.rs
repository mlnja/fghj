use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use include_dir::{include_dir, Dir};

use crate::{downloads, runs, store};

static UI_DIST: Dir = include_dir!("$CARGO_MANIFEST_DIR/ui/dist");

/// Per-workspace in-memory state: one of these lives behind an `Arc` in the
/// daemon's `WorkspaceRegistry`, shared across the per-request threads that
/// serve that workspace's routes. Durable state (runs, workspace identity)
/// lives in `db`, at `<path>/.fghj/fghj.db`, so it survives a `fghjd` restart.
pub struct WorkspaceState {
    pub path: PathBuf,
    pub db: Arc<store::WorkspaceDb>,
    pub runs: runs::RunRegistry,
    pub downloads: downloads::DownloadRegistry,
}

impl WorkspaceState {
    pub fn new(path: PathBuf) -> Result<Self> {
        let db = Arc::new(store::WorkspaceDb::open(&path)?);
        let runs = runs::RunRegistry::new(path.clone(), db.clone())?;
        Ok(Self {
            runs,
            downloads: downloads::DownloadRegistry::new(),
            db,
            path,
        })
    }
}

pub fn content_type_for(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") || path.ends_with(".mjs") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    }
}

pub fn json_response(value: serde_json::Value, status: u16) -> (Vec<u8>, &'static str, u16) {
    (value.to_string().into_bytes(), "application/json", status)
}

pub fn err_response(e: anyhow::Error) -> (Vec<u8>, &'static str, u16) {
    json_response(serde_json::json!({ "error": e.to_string() }), 500)
}

pub fn missing_workspace_response() -> (Vec<u8>, &'static str, u16) {
    json_response(
        serde_json::json!({ "error": "unknown or missing ?workspace=<id>; POST /workspaces first" }),
        400,
    )
}

/// Looks up `key` in a `k=v&k=v` query string. No percent-decoding — the
/// only values passed through this today (workspace ids, tail counts) never
/// need it.
pub fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Serves the embedded UI bundle, falling back to `index.html` for
/// unmatched routes (SPA-style).
pub fn static_response(route: &str) -> (Vec<u8>, &'static str, u16) {
    let trimmed = route.trim_start_matches('/');
    let file_path = if trimmed.is_empty() { "index.html" } else { trimmed };
    match UI_DIST.get_file(file_path) {
        Some(f) => (f.contents().to_vec(), content_type_for(file_path), 200),
        None => match UI_DIST.get_file("index.html") {
            Some(f) => (f.contents().to_vec(), "text/html; charset=utf-8", 200),
            None => (b"UI not built".to_vec(), "text/plain", 500),
        },
    }
}

pub fn respond(request: tiny_http::Request, (body, content_type, status): (Vec<u8>, &str, u16)) {
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
        .expect("valid header");
    let response = tiny_http::Response::from_data(body)
        .with_header(header)
        .with_status_code(status);
    let _ = request.respond(response);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_param_finds_key_among_siblings() {
        assert_eq!(query_param("workspace=abc&tail=50", "workspace"), Some("abc"));
        assert_eq!(query_param("workspace=abc&tail=50", "tail"), Some("50"));
        assert_eq!(query_param("workspace=abc&tail=50", "missing"), None);
        assert_eq!(query_param("", "workspace"), None);
    }

    #[test]
    fn content_type_matches_extension() {
        assert_eq!(content_type_for("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type_for("app.js"), "application/javascript; charset=utf-8");
        assert_eq!(content_type_for("styles.css"), "text/css; charset=utf-8");
        assert_eq!(content_type_for("universe.json"), "application/json");
        assert_eq!(content_type_for("logo.svg"), "image/svg+xml");
        assert_eq!(content_type_for("unknown.bin"), "application/octet-stream");
    }
}
