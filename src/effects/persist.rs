//! Mirrors workspace state into the workspace's SQLite store.

use std::sync::Arc;

use crate::persistence::WorkspaceDb;
use crate::state::{EnvState, WorkspaceState};

use super::AsyncEffect;

pub type Snapshot = Option<EnvState>;

pub struct PersistEffect {
    db: Arc<WorkspaceDb>,
    /// So an environment that leaves state is deleted, not rehydrated later.
    written: bool,
}

impl PersistEffect {
    pub fn new(db: Arc<WorkspaceDb>) -> Self {
        Self { db, written: false }
    }
}

impl AsyncEffect for PersistEffect {
    type Snapshot = Snapshot;

    fn extract(&self, state: &WorkspaceState) -> Self::Snapshot {
        state.env.clone()
    }

    async fn converge(&mut self, snapshot: Self::Snapshot) -> anyhow::Result<()> {
        match snapshot {
            // Not created yet: persisting it would resurrect an empty shell.
            Some(env) if env.network.is_empty() && env.containers.is_empty() => {}
            Some(env) => {
                self.db.clone().save_env(env).await?;
                self.written = true;
            }
            None if self.written => {
                self.db.clone().delete_env().await?;
                self.written = false;
            }
            None => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, Arc<WorkspaceDb>) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        (tmp, db)
    }

    fn created_env() -> EnvState {
        EnvState {
            network: "fghj-net".to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_created_environment_is_written_and_reloads() {
        let (_tmp, db) = db();
        let mut effect = PersistEffect::new(db.clone());
        effect.converge(Some(created_env())).await.unwrap();

        let loaded = db.clone().load_env().await.unwrap();
        assert_eq!(loaded, Some(created_env()));
    }

    #[tokio::test]
    async fn an_uncreated_environment_is_not_written() {
        let (_tmp, db) = db();
        let mut effect = PersistEffect::new(db.clone());
        effect.converge(Some(EnvState::default())).await.unwrap();
        assert!(db.clone().load_env().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_environment_that_leaves_state_is_deleted_from_the_database() {
        let (_tmp, db) = db();
        let mut effect = PersistEffect::new(db.clone());

        effect.converge(Some(created_env())).await.unwrap();
        assert!(db.clone().load_env().await.unwrap().is_some());

        effect.converge(None).await.unwrap();
        assert!(db.clone().load_env().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn deletion_is_limited_to_what_this_effect_wrote() {
        let (_tmp, db) = db();
        let mut effect = PersistEffect::new(db.clone());
        effect.converge(None).await.unwrap();
        assert!(!effect.written);
    }
}
