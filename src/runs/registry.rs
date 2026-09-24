use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::logs::capture_container_logs;
use crate::persistence::WorkspaceDb;

/// The Docker-facing half of run management: it owns the clients and
/// handles needed to *do* things (create a container, wait on a
/// healthcheck, stream logs) and the locks that keep those calls from
/// interleaving with themselves. It deliberately holds no run state.
///
/// Until migration phase 5 it kept its own `BTreeMap<String, RunState>`
/// alongside the actor's, and the two were reconciled by a poller. Two
/// copies of one fact is two chances to disagree, and every lifecycle call
/// had to remember to write both. Now the actor's published
/// `state::WorkspaceState` is the only store: callers pass in whatever
/// prior state a call needs and dispatch the result back as an action.
pub struct RunRegistry {
    pub(super) workspace: std::path::PathBuf,
    pub(super) db: Arc<WorkspaceDb>,
    pub(super) docker: Arc<bollard::Docker>,
    /// One lock per `(run_id, node_id)`, serializing that node's lifecycle
    /// calls — `restart_container` / `stop_container` / `remove_container`.
    ///
    /// Per node, not per workspace. Two concurrent calls for the *same* node
    /// both `docker::stop_and_remove` then recreate the same container name,
    /// which Docker itself rejects for whichever loses the race, so those
    /// genuinely must not interleave. Two calls for *different* nodes have
    /// no such conflict: they touch different containers, different volumes,
    /// different route entries. A workspace-wide lock conflated the two, and
    /// since these calls are held across `start_node` — which waits on a
    /// healthcheck — one slow node could block every other node's Start or
    /// Stop button in the workspace for the length of that wait.
    ///
    /// The inner mutexes are `tokio::sync::Mutex` because they are held
    /// across `.await`; the outer `std::sync::Mutex` only guards the brief
    /// lookup that hands one out, and is never held across an await.
    ///
    /// Serializing is all this does. Rejecting a genuine duplicate — a
    /// second "start" for a node already starting, which would otherwise
    /// just wait its turn and then redundantly stop-and-recreate the
    /// container the first call just started — happens upstream, in the
    /// reducer, off `ContainerInfo::pending_action`. This struct used to
    /// keep its own parallel `pending` map for that, a second gate on the
    /// same fact that could disagree with the first.
    pub(super) node_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Background per-`(run_id, node_id)` tasks streaming that node's
    /// *current* container's logs into `db` as they're produced — see
    /// `spawn_log_capture`. Keyed so that starting a node again aborts its
    /// previous capture task before replacing it, guaranteeing at most one
    /// task (and one open generation) writing for a given node at a time.
    pub(super) log_captures: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl RunRegistry {
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
        }
    }

    /// Starts (or restarts) background capture of `container_name`'s logs
    /// into `db`, opening a fresh generation for `(run_id, node_id)`. Called
    /// from `start_node` — the single choke point every container-creating
    /// path (`start`, `ensure_running`, `restart_container`) already funnels
    /// through — so every container gets its own capture task without each
    /// caller needing to remember to set one up.
    ///
    /// Aborts any capture task already running for this node first: normally
    /// the old task would already have ended on its own (the previous
    /// container's log stream ends when Docker removes it, which
    /// `start_node`'s callers always do before recreating), but aborting
    /// explicitly avoids ever leaving two tasks writing under the same key.
    pub(super) fn spawn_log_capture(&self, run_id: &str, node_id: &str, container_name: &str) {
        let key = format!("{run_id}:{node_id}");
        let handle = tokio::spawn(capture_container_logs(
            self.db.clone(),
            self.docker.clone(),
            run_id.to_string(),
            node_id.to_string(),
            container_name.to_string(),
        ));
        if let Some(old) = self.log_captures.lock().unwrap().insert(key, handle) {
            old.abort();
        }
    }

    /// The lifecycle lock for one node, created on first use. Held across
    /// the whole of a `restart`/`stop`/`remove` so that node's Docker calls
    /// never interleave with themselves — see `node_locks`.
    pub(super) fn node_lock(&self, run_id: &str, node_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.node_locks
            .lock()
            .unwrap()
            .entry(format!("{run_id}:{node_id}"))
            .or_default()
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::WorkspaceDb;
    use std::time::Duration;

    /// A registry with no runs, backed by a throwaway database. The docker
    /// client is never dialled by anything below — these tests are about the
    /// locking and merge rules, which are pure bookkeeping.
    async fn empty_registry() -> (tempfile::TempDir, RunRegistry) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        let docker = Arc::new(
            bollard::Docker::connect_with_local_defaults().expect("docker client construction"),
        );
        let registry = RunRegistry::new(tmp.path().to_path_buf(), db, docker);
        (tmp, registry)
    }

    #[tokio::test]
    async fn the_same_node_gets_the_same_lock_and_a_different_node_does_not() {
        let (_tmp, registry) = empty_registry().await;
        let a1 = registry.node_lock("default", "api.svc");
        let a2 = registry.node_lock("default", "api.svc");
        let b = registry.node_lock("default", "db.svc");
        assert!(Arc::ptr_eq(&a1, &a2));
        assert!(!Arc::ptr_eq(&a1, &b));
    }

    /// The same node id in two different runs is two different containers,
    /// so it must not share a lock either.
    #[tokio::test]
    async fn the_same_node_in_two_runs_gets_two_locks() {
        let (_tmp, registry) = empty_registry().await;
        let a = registry.node_lock("default", "api.svc");
        let b = registry.node_lock("preview", "api.svc");
        assert!(!Arc::ptr_eq(&a, &b));
    }

    /// B12 itself: one node stuck in a long lifecycle call must not hold up
    /// any other node's. Under the old workspace-wide `action_lock` the
    /// second acquisition here would have blocked for the length of the
    /// first — up to a full healthcheck wait.
    #[tokio::test]
    async fn a_node_held_for_a_long_time_does_not_block_a_different_node() {
        let (_tmp, registry) = empty_registry().await;
        let slow = registry.node_lock("default", "slow.svc");
        let held = slow.lock_owned().await;

        let other = registry.node_lock("default", "other.svc");
        let free = tokio::time::timeout(Duration::from_secs(1), other.lock())
            .await
            .expect("a different node's lock must be free while one node is busy");
        drop(free);
        drop(held);
    }

    /// The other half of the same rule: two calls for the *same* node still
    /// have to take turns, because they both stop-and-recreate one container
    /// name and Docker rejects whichever loses that race.
    #[tokio::test]
    async fn two_calls_for_one_node_still_take_turns() {
        let (_tmp, registry) = empty_registry().await;
        let first = registry.node_lock("default", "api.svc");
        let held = first.lock_owned().await;

        let second = registry.node_lock("default", "api.svc");
        let blocked = tokio::time::timeout(Duration::from_millis(200), second.lock()).await;
        assert!(blocked.is_err(), "same node must serialize");
        drop(held);
        let free = tokio::time::timeout(Duration::from_secs(1), second.lock())
            .await
            .expect("lock must be free once the first call finishes");
        drop(free);
    }
}
