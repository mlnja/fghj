use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use anyhow::Result;

use super::logs::capture_container_logs;
use crate::persistence::{self, WorkspaceDb};
use crate::state::RunState;

pub struct RunRegistry {
    pub(super) workspace: std::path::PathBuf,
    pub(super) db: Arc<WorkspaceDb>,
    pub(super) docker: Arc<bollard::Docker>,
    pub(super) runs: Mutex<BTreeMap<String, RunState>>,
    /// Serializes `restart_container`/`stop_container`/`remove_container`
    /// across the *whole workspace*, not per node: two concurrent lifecycle
    /// calls for the same node both `docker::stop_and_remove` then recreate
    /// the same container name, which Docker itself will reject for
    /// whichever loses the race, and interleaving two *different* nodes'
    /// Docker calls arbitrarily isn't obviously safe either. An `Arc`'d
    /// workspace-wide `tokio::sync::Mutex` (must survive an `.await`, unlike
    /// the plain `std::sync::Mutex` above that only ever guards a quick
    /// snapshot/write-back) makes concurrent calls run one at a time instead
    /// of racing.
    ///
    /// Serializing is all this does. Rejecting a genuine duplicate — a
    /// second "start" for a node already starting, which would otherwise
    /// just wait its turn and then redundantly stop-and-recreate the
    /// container the first call just started — happens upstream, in the
    /// reducer, off `ContainerInfo::pending_action`. This struct used to
    /// keep its own parallel `pending` map for that, a second gate on the
    /// same fact that could disagree with the first.
    pub(super) action_lock: tokio::sync::Mutex<()>,
    /// Background per-`(run_id, node_id)` tasks streaming that node's
    /// *current* container's logs into `db` as they're produced — see
    /// `spawn_log_capture`. Keyed so that starting a node again aborts its
    /// previous capture task before replacing it, guaranteeing at most one
    /// task (and one open generation) writing for a given node at a time.
    pub(super) log_captures: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl RunRegistry {
    /// Loads any runs persisted from a previous `fghjd` lifetime and
    /// reconciles each against real docker state: a run whose containers are
    /// all still alive is restored with freshly-inspected statuses, and a run
    /// missing any container (removed out-of-band, or lost across a reboot
    /// with no restart policy) is dropped rather than presented as running.
    pub async fn new(
        workspace: std::path::PathBuf,
        db: Arc<WorkspaceDb>,
        docker: Arc<bollard::Docker>,
    ) -> Result<Self> {
        let reconciled = persistence::rehydrate(db.clone(), docker.clone()).await?;
        Ok(Self {
            workspace,
            db,
            docker,
            runs: Mutex::new(reconciled),
            action_lock: tokio::sync::Mutex::new(()),
            log_captures: Mutex::new(HashMap::new()),
        })
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

    /// Every live run this registry is currently tracking. `pending_action`
    /// is always `None` here: an in-flight action is the reducer's to
    /// record, and this is only ever read to *seed* reducer state (see
    /// `daemon::WorkspaceRegistry::wire_actor`) or to enumerate runs for
    /// teardown, never as the authority on what a node is doing.
    pub fn list(&self) -> Vec<RunState> {
        self.runs.lock().unwrap().values().cloned().collect()
    }
}
