use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::activity::ActivityLog;
use crate::{orchestrator, persistence};

pub struct WorkspaceState {
    pub id: String,
    pub path: PathBuf,
    pub db: Arc<persistence::WorkspaceDb>,
    pub docker: Arc<bollard::Docker>,
    pub orchestrator: orchestrator::Orchestrator,
    /// The daemon's, shared by every workspace.
    pub activities: Arc<ActivityLog>,
    /// Read once, to seed the actor; never again. Here because `Orchestrator`
    /// holds no run state.
    pub rehydrated: Option<crate::state::EnvState>,
}

impl WorkspaceState {
    pub async fn new(
        id: String,
        path: PathBuf,
        docker: Arc<bollard::Docker>,
        activities: Arc<ActivityLog>,
    ) -> Result<Self> {
        let db = Arc::new(persistence::WorkspaceDb::open(&path)?);
        let rehydrated = persistence::rehydrate(db.clone(), docker.clone()).await?;
        let orchestrator =
            orchestrator::Orchestrator::new(path.clone(), db.clone(), docker.clone());
        Ok(Self {
            orchestrator,
            rehydrated,
            activities,
            id,
            db,
            docker,
            path,
        })
    }
}
