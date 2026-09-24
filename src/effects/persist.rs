//! Mirrors workspace state into the per-workspace SQLite store.
//!
//! Persistence used to be a hand-placed `save_run` after every mutation in
//! `runs/`, each one writing a `RunState` snapshot taken before the Docker
//! work it was recording. Two concurrent per-node operations could
//! therefore each persist a view that had already lost the other's
//! container.
//!
//! As an effect it is derived, not remembered: whatever the actor has
//! published is what lands in the database, and the reducer is the only
//! thing that decides what that is.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::persistence::WorkspaceDb;
use crate::state::{RunState, WorkspaceState};

use super::AsyncEffect;

pub type Snapshot = BTreeMap<String, RunState>;

pub struct PersistEffect {
    db: Arc<WorkspaceDb>,
    /// Run ids this effect has written, so one that disappears from state
    /// gets deleted rather than lingering to be rehydrated on next start.
    known: Vec<String>,
}

impl PersistEffect {
    pub fn new(db: Arc<WorkspaceDb>) -> Self {
        Self {
            db,
            known: Vec::new(),
        }
    }
}

impl AsyncEffect for PersistEffect {
    type Snapshot = Snapshot;

    fn extract(&self, state: &WorkspaceState) -> Self::Snapshot {
        state.runs.clone()
    }

    async fn converge(&mut self, snapshot: Self::Snapshot) -> anyhow::Result<()> {
        for run in snapshot.values() {
            // A run with no containers and no network has not been created
            // yet — persisting it would resurrect an empty shell on the
            // next daemon start.
            if run.network.is_empty() && run.containers.is_empty() {
                continue;
            }
            self.db.clone().save_run(run.clone()).await?;
        }
        for run_id in std::mem::take(&mut self.known) {
            if !snapshot.contains_key(&run_id) {
                self.db.clone().delete_run(run_id).await?;
            }
        }
        self.known = snapshot.keys().cloned().collect();
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

    fn created_run(run_id: &str) -> RunState {
        RunState {
            run_id: run_id.to_string(),
            network: format!("fghj-net-{run_id}"),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_created_run_is_written_and_reloads() {
        let (_tmp, db) = db();
        let mut effect = PersistEffect::new(db.clone());
        let mut snapshot = Snapshot::new();
        snapshot.insert("default".into(), created_run("default"));
        effect.converge(snapshot).await.unwrap();

        let loaded = db.clone().load_runs().await.unwrap();
        assert!(loaded.contains_key("default"), "{loaded:?}");
    }

    /// A run that has been planned but never created has nothing worth
    /// remembering — writing it would make the next daemon start rehydrate
    /// an empty run that never existed.
    #[tokio::test]
    async fn an_uncreated_run_is_not_written() {
        let (_tmp, db) = db();
        let mut effect = PersistEffect::new(db.clone());
        let mut snapshot = Snapshot::new();
        snapshot.insert(
            "default".into(),
            RunState {
                run_id: "default".into(),
                ..Default::default()
            },
        );
        effect.converge(snapshot).await.unwrap();
        assert!(db.clone().load_runs().await.unwrap().is_empty());
    }

    /// Stopping a run removes it from state; the database has to follow, or
    /// the next start would rehydrate a run the user just tore down.
    #[tokio::test]
    async fn a_run_that_leaves_state_is_deleted_from_the_database() {
        let (_tmp, db) = db();
        let mut effect = PersistEffect::new(db.clone());

        let mut snapshot = Snapshot::new();
        snapshot.insert("default".into(), created_run("default"));
        effect.converge(snapshot).await.unwrap();
        assert!(!db.clone().load_runs().await.unwrap().is_empty());

        effect.converge(Snapshot::new()).await.unwrap();
        assert!(db.clone().load_runs().await.unwrap().is_empty());
    }

    /// Only runs this effect actually wrote get deleted — it must not issue
    /// a delete for every id it has never seen.
    #[tokio::test]
    async fn deletion_is_limited_to_runs_this_effect_wrote() {
        let (_tmp, db) = db();
        let mut effect = PersistEffect::new(db.clone());
        effect.converge(Snapshot::new()).await.unwrap();
        assert!(effect.known.is_empty());
    }
}
