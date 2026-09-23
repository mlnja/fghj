use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use include_dir::{Dir, include_dir};

use crate::util::mime::content_type_for;
use crate::{downloads, persistence, runs};

/// `pub(crate)` so `sidecar_image` can reuse this same embedded copy when
/// materializing the sidecar's Docker build context, rather than embedding
/// the UI a second time.
pub(crate) static UI_DIST: Dir = include_dir!("$CARGO_MANIFEST_DIR/ui/dist");

/// Per-workspace in-memory state: one of these lives behind an `Arc` in the
/// daemon's `WorkspaceRegistry`, shared across the concurrent request tasks
/// that serve that workspace's routes. Durable state (runs, workspace
/// identity) lives in `db`, at `<path>/.fghj/fghj.db`, so it survives a
/// `fghjd` restart.
pub struct WorkspaceState {
    pub path: PathBuf,
    pub db: Arc<persistence::WorkspaceDb>,
    pub docker: Arc<bollard::Docker>,
    pub runs: runs::RunRegistry,
    pub downloads: downloads::DownloadRegistry,
}

impl WorkspaceState {
    pub async fn new(path: PathBuf, docker: Arc<bollard::Docker>) -> Result<Self> {
        let db = Arc::new(persistence::WorkspaceDb::open(&path)?);
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

/// Serves the embedded UI bundle, falling back to `index.html` for
/// unmatched routes (SPA-style).
pub fn static_response(route: &str) -> (Vec<u8>, &'static str, u16) {
    let trimmed = route.trim_start_matches('/');
    let file_path = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };
    match UI_DIST.get_file(file_path) {
        Some(f) => (f.contents().to_vec(), content_type_for(file_path), 200),
        None => match UI_DIST.get_file("index.html") {
            Some(f) => (f.contents().to_vec(), "text/html; charset=utf-8", 200),
            None => (b"UI not built".to_vec(), "text/plain", 500),
        },
    }
}
