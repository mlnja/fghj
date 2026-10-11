use anyhow::Result;

use crate::docker;
use crate::state::{ContainerInfo, EnvState};

use super::registry::Orchestrator;
use super::route_table::sidecar_routes_dir;

impl Orchestrator {
    /// Tears the environment down. Volumes are kept.
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

    /// Stops one node's container without removing it.
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
        let status = match docker::inspect_status(&self.docker, &container_name).await {
            Ok(Some(s)) => s.status,
            _ => "exited".to_string(),
        };
        ev.ok("stopping container").await;
        let mut updated = container.clone();
        updated.desired.running = false;
        updated.observed.status = status;
        updated.pending_action = None;
        Ok(updated)
    }

    /// Stops and removes one node's container. Volumes are kept.
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
