use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::{downloads, orchestrator, persistence};

pub struct WorkspaceState {
    pub path: PathBuf,
    pub db: Arc<persistence::WorkspaceDb>,
    pub docker: Arc<bollard::Docker>,
    pub orchestrator: orchestrator::Orchestrator,
    pub downloads: downloads::DownloadRegistry,
    /// Read once, to seed the actor; never again. Here because `Orchestrator`
    /// holds no run state.
    pub rehydrated: Option<crate::state::EnvState>,
}

impl WorkspaceState {
    pub async fn new(path: PathBuf, docker: Arc<bollard::Docker>) -> Result<Self> {
        let db = Arc::new(persistence::WorkspaceDb::open(&path)?);
        let rehydrated = persistence::rehydrate(db.clone(), docker.clone()).await?;
        let orchestrator =
            orchestrator::Orchestrator::new(path.clone(), db.clone(), docker.clone());
        Ok(Self {
            orchestrator,
            rehydrated,
            downloads: downloads::DownloadRegistry::new(),
            db,
            docker,
            path,
        })
    }
}
