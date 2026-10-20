use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::logs::capture_container_logs;
use crate::persistence::WorkspaceDb;
use crate::supervisor;

/// Docker-facing clients and locks. Holds no state: callers pass in what they
/// need and dispatch the result back to the actor.
pub struct Orchestrator {
    pub(super) workspace: std::path::PathBuf,
    pub(super) db: Arc<WorkspaceDb>,
    pub(super) docker: Arc<bollard::Docker>,
    /// Per node, not per workspace: two calls for one node would both recreate the
    /// same container name, but different nodes don't conflict, and one slow
    /// healthcheck mustn't block every other node. Duplicate starts are rejected
    /// by the reducer, not here.
    pub(super) node_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// At most one capture task per node.
    pub(super) log_captures: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    /// A create and the reconciler can both bring the sidecar up, and starting
    /// one removes whatever holds its name first.
    pub(super) sidecar_lock: tokio::sync::Mutex<()>,
}

impl Orchestrator {
    pub fn new(
        workspace: std::path::PathBuf,
        db: Arc<WorkspaceDb>,
        docker: Arc<bollard::Docker>,
    ) -> Self {
        Self {
            workspace,
            db,
            docker,
            node_locks: Mutex::new(HashMap::new()),
            log_captures: Mutex::new(HashMap::new()),
            sidecar_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Aborts any previous capture for the node so two tasks never write under the
    /// same key.
    pub(super) fn spawn_log_capture(
        &self,
        node_id: &str,
        container_name: &str,
        from: crate::docker::LogsFrom,
    ) {
        let handle = supervisor::supervise(
            format!("log capture[{node_id}]"),
            capture_container_logs(
                self.db.clone(),
                self.docker.clone(),
                node_id.to_string(),
                container_name.to_string(),
                from,
            ),
        );
        if let Some(old) = self
            .log_captures
            .lock()
            .unwrap()
            .insert(node_id.to_string(), handle)
        {
            old.abort();
        }
    }

    pub(super) fn node_lock(&self, node_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.node_locks
            .lock()
            .unwrap()
            .entry(node_id.to_string())
            .or_default()
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::WorkspaceDb;
    use std::time::Duration;

    async fn empty_registry() -> (tempfile::TempDir, Orchestrator) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        let docker = Arc::new(crate::docker::undialled_client());
        let registry = Orchestrator::new(tmp.path().to_path_buf(), db, docker);
        (tmp, registry)
    }

    #[tokio::test]
    async fn the_same_node_gets_the_same_lock_and_a_different_node_does_not() {
        let (_tmp, registry) = empty_registry().await;
        let a1 = registry.node_lock("api.svc");
        let a2 = registry.node_lock("api.svc");
        let b = registry.node_lock("db.svc");
        assert!(Arc::ptr_eq(&a1, &a2));
        assert!(!Arc::ptr_eq(&a1, &b));
    }

    #[tokio::test]
    async fn a_node_held_for_a_long_time_does_not_block_a_different_node() {
        let (_tmp, registry) = empty_registry().await;
        let slow = registry.node_lock("slow.svc");
        let held = slow.lock_owned().await;

        let other = registry.node_lock("other.svc");
        let free = tokio::time::timeout(Duration::from_secs(1), other.lock())
            .await
            .expect("a different node's lock must be free while one node is busy");
        drop(free);
        drop(held);
    }

    #[tokio::test]
    async fn two_calls_for_one_node_still_take_turns() {
        let (_tmp, registry) = empty_registry().await;
        let first = registry.node_lock("api.svc");
        let held = first.lock_owned().await;

        let second = registry.node_lock("api.svc");
        let blocked = tokio::time::timeout(Duration::from_millis(200), second.lock()).await;
        assert!(blocked.is_err(), "same node must serialize");
        drop(held);
        let free = tokio::time::timeout(Duration::from_secs(1), second.lock())
            .await
            .expect("lock must be free once the first call finishes");
        drop(free);
    }
}
