//! Per-node lifecycle commands: get, stop, restart, remove.

use anyhow::{Result, bail};

use super::naming::DEFAULT_RUN_ID;
use crate::docker;
use crate::resolver::Graph;
use crate::state::{ContainerInfo, RunState};
use crate::util::label::sanitize_label;

use super::health::HealthBudget;
use super::registry::RunRegistry;
use super::route_table::sidecar_routes_dir;

impl RunRegistry {
    /// Tears a whole run down: every container, the sidecar, the network,
    /// and — for a named run only — its volumes.
    ///
    /// Takes the run rather than looking it up: the reducer owns the only
    /// copy, and the caller (`effects::docker::converge`) already has it.
    /// Removing it from state is that caller's job too, by way of
    /// `Action::RunTeardownSettled`, which is in turn what makes
    /// `effects::persist` drop the database row and `effects::routes`
    /// delete the sidecar route table.
    pub async fn stop(&self, run_id: &str, state: &RunState) -> Result<()> {
        for c in state.containers.values() {
            self.begin_event_cycle(run_id, &c.node_id, "stop").await;
            self.record_event(
                run_id,
                &c.node_id,
                "stop",
                "stopping container",
                "running",
                None,
            )
            .await;
            docker::stop_and_remove(&self.docker, &c.desired.container_name).await;
            self.record_event(run_id, &c.node_id, "stop", "stopping container", "ok", None)
                .await;
        }
        if !state.sidecar_container_name.is_empty() {
            docker::stop_and_remove(&self.docker, &state.sidecar_container_name).await;
        }
        let _ = std::fs::remove_dir_all(sidecar_routes_dir(&state.network));
        docker::remove_network(&self.docker, &state.network).await;
        // The default run's `scope: "run"` volumes get the exact same
        // derived name on every start (`derive_volume_name` only folds the
        // run id in for a *named* run) — deleting them here would silently
        // wipe data a plain stop+restart expects to still be there. Only a
        // named/preview run's volumes are safe to clean up.
        if run_id != DEFAULT_RUN_ID {
            docker::remove_run_scoped_volumes(&self.docker, run_id).await;
        }
        Ok(())
    }

    /// (Re)starts a single node's container within an already-running run —
    /// the per-node counterpart to `start`/`ensure_running`'s whole-run
    /// granularity, backing the Drawer's "Start" button. Always recreates
    /// from scratch (mirrors `ensure_running`'s stop-then-start dance) so a
    /// `.fghj.yaml` change since the container last started is actually
    /// picked up, rather than silently no-op'ing on an already-running one.
    pub async fn restart_container(
        &self,
        graph: &Graph,
        run_id: &str,
        node_id: &str,
        network: &str,
        sidecar_ip: Option<&str>,
    ) -> Result<ContainerInfo> {
        let lock = self.node_lock(run_id, node_id);
        let _guard = lock.lock().await;
        let Some(node) = graph.nodes.iter().find(|n| n.id == node_id) else {
            bail!("no such node: {node_id}");
        };
        let container_name = format!(
            "fghj-{}-{}-{}",
            sanitize_label(&graph.workspace_name),
            run_id,
            sanitize_label(&node.id)
        );
        docker::stop_and_remove(&self.docker, &container_name).await;

        // Restarting one node is not sharing a budget with anything, so it
        // gets a full per-node health allowance.
        self.start_node(
            graph,
            node,
            run_id,
            network,
            sidecar_ip,
            &HealthBudget::single_node(),
        )
        .await
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
        run_id: &str,
        node_id: &str,
        container: &ContainerInfo,
    ) -> Result<ContainerInfo> {
        let lock = self.node_lock(run_id, node_id);
        let _guard = lock.lock().await;
        let container_name = container.desired.container_name.clone();
        self.begin_event_cycle(run_id, node_id, "stop").await;
        self.record_event(
            run_id,
            node_id,
            "stop",
            "stopping container",
            "running",
            None,
        )
        .await;
        docker::stop_container(&self.docker, &container_name).await;
        let status = match docker::inspect_status(&self.docker, &container_name, "").await {
            Ok(Some(s)) => s.status,
            _ => "exited".to_string(),
        };
        self.record_event(run_id, node_id, "stop", "stopping container", "ok", None)
            .await;
        let mut updated = container.clone();
        // Stopping is itself the recorded intent, so `desired` moves too —
        // this is the one place a stopped container is *supposed* to be
        // stopped, as opposed to having died on its own.
        updated.desired.running = false;
        updated.observed.status = status;
        Ok(updated)
    }

    /// Stops and removes a single node's container, dropping it from the
    /// run entirely — the Drawer's "Delete" button. Named volumes survive
    /// (same reasoning as `stop`'s default-run carve-out: a volume's whole
    /// point is to outlive any one container), so a later "Start" click
    /// picks the data back up in a fresh container.
    pub async fn remove_container(
        &self,
        run_id: &str,
        node_id: &str,
        container: &ContainerInfo,
    ) -> Result<()> {
        let lock = self.node_lock(run_id, node_id);
        let _guard = lock.lock().await;
        let container_name = container.desired.container_name.clone();
        self.begin_event_cycle(run_id, node_id, "stop").await;
        self.record_event(
            run_id,
            node_id,
            "stop",
            "removing container",
            "running",
            None,
        )
        .await;
        docker::stop_and_remove(&self.docker, &container_name).await;
        self.record_event(run_id, node_id, "stop", "removing container", "ok", None)
            .await;
        Ok(())
    }
}
