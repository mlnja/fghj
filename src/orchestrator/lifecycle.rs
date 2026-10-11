//! Environment teardown and per-node stop/remove. Starting goes through
//! `orchestrate`.

use anyhow::Result;

use crate::docker;
use crate::state::{ContainerInfo, EnvState};

use super::registry::Orchestrator;
use super::route_table::sidecar_routes_dir;

impl Orchestrator {
    /// Tears the environment down: every container, the sidecar and the
    /// network. Volumes are kept — they hold the data the next start
    /// expects to find.
    ///
    /// Takes the run rather than looking it up: the reducer owns the only
    /// copy, and the caller (`daemon::WorkspaceRegistry::stop`, as the
    /// workspace's actor goes away) already has it.
    pub async fn stop(&self, state: &EnvState) -> Result<()> {
        for c in state.containers.values() {
            let ev = self.events(&c.node_id, "stop");
            ev.begin().await;
            ev.running("stopping container").await;
            docker::stop_and_remove(&self.docker, &c.desired.container_name).await;
            ev.ok("stopping container").await;
        }
        if !state.sidecar_container_name.is_empty() {
            docker::stop_and_remove(&self.docker, &state.sidecar_container_name).await;
        }
        let _ = std::fs::remove_dir_all(sidecar_routes_dir(&state.network));
        docker::remove_network(&self.docker, &state.network).await;
        Ok(())
    }

    /// Stops a single node's container without removing it or touching the
    /// rest of the run — the Drawer's "Stop" button. Unlike `stop` (whole
    /// run), this leaves the container itself and its named volumes in
    /// place; a subsequent "Start" click just recreates it.
    ///
    /// Returns the container as it now stands rather than `()`: the caller
    /// reports that straight into the reducer, so there is no read-back
    /// from a second copy of the run to disagree with.
    pub async fn stop_container(
        &self,
        node_id: &str,
        container: &ContainerInfo,
    ) -> Result<ContainerInfo> {
        let lock = self.node_lock(node_id);
        let _guard = lock.lock().await;
        let container_name = container.desired.container_name.clone();
        let ev = self.events(node_id, "stop");
        ev.begin().await;
        ev.running("stopping container").await;
        docker::stop_container(&self.docker, &container_name).await;
        let status = match docker::inspect_status(&self.docker, &container_name, "").await {
            Ok(Some(s)) => s.status,
            _ => "exited".to_string(),
        };
        ev.ok("stopping container").await;
        let mut updated = container.clone();
        // Stopping is itself the recorded intent, so `desired` moves too —
        // this is the one place a stopped container is *supposed* to be
        // stopped, as opposed to having died on its own.
        updated.desired.running = false;
        updated.observed.status = status;
        // `container` was cloned mid-action, so it still carries the
        // `Stopping` mark that got us here. The reducer clears it on settle
        // regardless, but a method that reports a container as stopped
        // should not also report an action still pending on it.
        updated.pending_action = None;
        Ok(updated)
    }

    /// Stops and removes a single node's container, dropping it from the
    /// run entirely — the Drawer's "Delete" button. Named volumes survive
    /// (same reasoning as `stop`'s default-run carve-out: a volume's whole
    /// point is to outlive any one container), so a later "Start" click
    /// picks the data back up in a fresh container.
    pub async fn remove_container(&self, node_id: &str, container: &ContainerInfo) -> Result<()> {
        let lock = self.node_lock(node_id);
        let _guard = lock.lock().await;
        let container_name = container.desired.container_name.clone();
        let ev = self.events(node_id, "stop");
        ev.begin().await;
        ev.running("removing container").await;
        docker::stop_and_remove(&self.docker, &container_name).await;
        ev.ok("removing container").await;
        Ok(())
    }
}
