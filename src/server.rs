use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::{downloads, persistence, runs};

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
