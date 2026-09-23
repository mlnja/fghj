//! Per-node lifecycle commands: get, stop, restart, remove.

use anyhow::{Result, bail};

use super::naming::DEFAULT_RUN_ID;
use super::route_table::{sidecar_routes_dir, write_route_table};
use crate::docker;
use crate::resolver::Graph;
use crate::state::{ContainerInfo, RunState};
use crate::util::label::sanitize_label;

use super::registry::RunRegistry;

impl RunRegistry {
    pub fn get(&self, run_id: &str) -> Option<RunState> {
        self.runs.lock().unwrap().get(run_id).cloned()
    }

    pub async fn stop(&self, run_id: &str) -> Result<()> {
        let state = {
            let mut runs = self.runs.lock().unwrap();
            let Some(state) = runs.remove(run_id) else {
                bail!("no such run: {run_id}");
            };
            state
        };
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
        self.db.clone().delete_run(run_id.to_string()).await?;
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
    ) -> Result<ContainerInfo> {
        let _guard = self.action_lock.lock().await;
        let mut state = {
            let runs = self.runs.lock().unwrap();
            let Some(state) = runs.get(run_id) else {
                bail!("no such run: {run_id}");
            };
            state.clone()
        };
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

        let info = self
            .start_node(
                graph,
                node,
                run_id,
                &state.network,
                state.sidecar_ip.as_deref(),
            )
            .await?;
        state.containers.insert(info.node_id.clone(), info.clone());
        self.db.clone().save_run(state.clone()).await?;
        if let Err(e) = write_route_table(&state) {
            eprintln!("fghjd: failed to write sidecar route table for run {run_id}: {e:#}");
        }
        self.runs.lock().unwrap().insert(run_id.to_string(), state);
        Ok(info)
    }

    /// Stops a single node's container without removing it or touching the
    /// rest of the run — the Drawer's "Stop" button. Unlike `stop` (whole
    /// run), this leaves the container itself and its named volumes in
    /// place; a subsequent "Start" click just recreates it.
    pub async fn stop_container(&self, run_id: &str, node_id: &str) -> Result<()> {
        let _guard = self.action_lock.lock().await;
        let mut state = {
            let runs = self.runs.lock().unwrap();
            let Some(state) = runs.get(run_id) else {
                bail!("no such run: {run_id}");
            };
            state.clone()
        };
        let Some(c) = state.containers.get_mut(node_id) else {
            bail!("no such node in run {run_id}: {node_id}");
        };
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
        let container_name = c.desired.container_name.clone();
        docker::stop_container(&self.docker, &container_name).await;
        // Stopping is itself the recorded intent, so `desired` moves too —
        // this is the one place a stopped container is *supposed* to be
        // stopped, as opposed to having died on its own.
        c.desired.running = false;
        c.observed.status = match docker::inspect_status(&self.docker, &container_name, "").await {
            Ok(Some(s)) => s.status,
            _ => "exited".to_string(),
        };
        self.record_event(run_id, node_id, "stop", "stopping container", "ok", None)
            .await;
        self.db.clone().save_run(state.clone()).await?;
        if let Err(e) = write_route_table(&state) {
            eprintln!("fghjd: failed to write sidecar route table for run {run_id}: {e:#}");
        }
        self.runs.lock().unwrap().insert(run_id.to_string(), state);
        Ok(())
    }

    /// Stops and removes a single node's container, dropping it from the
    /// run entirely — the Drawer's "Delete" button. Named volumes survive
    /// (same reasoning as `stop`'s default-run carve-out: a volume's whole
    /// point is to outlive any one container), so a later "Start" click
    /// picks the data back up in a fresh container.
    pub async fn remove_container(&self, run_id: &str, node_id: &str) -> Result<()> {
        let _guard = self.action_lock.lock().await;
        let mut state = {
            let runs = self.runs.lock().unwrap();
            let Some(state) = runs.get(run_id) else {
                bail!("no such run: {run_id}");
            };
            state.clone()
        };
        let Some(c) = state.containers.remove(node_id) else {
            bail!("no such node in run {run_id}: {node_id}");
        };
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
        docker::stop_and_remove(&self.docker, &c.desired.container_name).await;
        self.record_event(run_id, node_id, "stop", "removing container", "ok", None)
            .await;
        self.db.clone().save_run(state.clone()).await?;
        if let Err(e) = write_route_table(&state) {
            eprintln!("fghjd: failed to write sidecar route table for run {run_id}: {e:#}");
        }
        self.runs.lock().unwrap().insert(run_id.to_string(), state);
        Ok(())
    }
}
