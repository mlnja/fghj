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
    /// What a previous `fghjd` lifetime left running, reconciled against
    /// real Docker state once, here, at the moment this workspace is
    /// constructed. Read exactly once — by `daemon::WorkspaceRegistry::wire_actor`,
    /// to seed the actor — and never consulted again: from that point the
    /// actor's published `WorkspaceState` is the only record of what runs
    /// exist. It lives here rather than inside `RunRegistry` because
    /// `RunRegistry` deliberately holds no run state at all.
    pub rehydrated: std::collections::BTreeMap<String, crate::state::RunState>,
}

impl WorkspaceState {
    pub async fn new(path: PathBuf, docker: Arc<bollard::Docker>) -> Result<Self> {
        let db = Arc::new(persistence::WorkspaceDb::open(&path)?);
        let rehydrated = persistence::rehydrate(db.clone(), docker.clone()).await?;
        let runs = runs::RunRegistry::new(path.clone(), db.clone(), docker.clone());
        Ok(Self {
            runs,
            rehydrated,
            downloads: downloads::DownloadRegistry::new(),
            db,
            docker,
            path,
        })
    }
}
