use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use include_dir::{include_dir, Dir};

use crate::{downloads, runs, store};

static UI_DIST: Dir = include_dir!("$CARGO_MANIFEST_DIR/ui/dist");

/// Per-workspace in-memory state: one of these lives behind an `Arc` in the
/// daemon's `WorkspaceRegistry`, shared across the concurrent request tasks
/// that serve that workspace's routes. Durable state (runs, workspace
/// identity) lives in `db`, at `<path>/.fghj/fghj.db`, so it survives a
/// `fghjd` restart.
pub struct WorkspaceState {
    pub path: PathBuf,
    pub db: Arc<store::WorkspaceDb>,
    pub docker: Arc<bollard::Docker>,
    pub runs: runs::RunRegistry,
    pub downloads: downloads::DownloadRegistry,
}

impl WorkspaceState {
    pub async fn new(path: PathBuf, docker: Arc<bollard::Docker>) -> Result<Self> {
        let db = Arc::new(store::WorkspaceDb::open(&path)?);
        let runs = runs::RunRegistry::new(path.clone(), db.clone(), docker.clone()).await?;
        Ok(Self {
            runs,
            downloads: downloads::DownloadRegistry::new(),
            db,
            docker,
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

#[cfg(test)]
mod tests {
    use super::*;

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
