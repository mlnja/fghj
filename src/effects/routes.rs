//! Renders the sidecar's `routes.json` from published state, so it's never
//! written from a stale snapshot.

use crate::orchestrator::route_table::{sidecar_routes_dir, write_route_table};
use crate::state::{EnvState, WorkspaceState};

use super::Effect;

pub type Snapshot = Option<EnvState>;

#[derive(Default)]
pub struct RouteTableEffect {
    /// So a vanished environment's table is removed, not served on.
    written: Option<String>,
}

impl RouteTableEffect {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Effect for RouteTableEffect {
    type Snapshot = Snapshot;

    fn extract(&self, state: &WorkspaceState) -> Self::Snapshot {
        state.env.clone()
    }

    fn converge(&mut self, snapshot: &Self::Snapshot) -> anyhow::Result<()> {
        let network = snapshot
            .as_ref()
            .filter(|env| !env.network.is_empty())
            .map(|env| env.network.clone());
        if let Some(env) = snapshot.as_ref().filter(|_| network.is_some()) {
            write_route_table(env)?;
        }
        if let Some(old) = self.written.take()
            && network.as_ref() != Some(&old)
        {
            let _ = std::fs::remove_dir_all(sidecar_routes_dir(&old));
        }
        self.written = network;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ContainerDesired, ContainerInfo, PortRoute};
    use std::collections::BTreeMap;

    fn env_with_route(network: &str, domain: &str) -> EnvState {
        let mut containers = BTreeMap::new();
        containers.insert(
            "web".to_string(),
            ContainerInfo {
                node_id: "web".to_string(),
                desired: ContainerDesired {
                    running: true,
                    container_name: "fghj-web".to_string(),
                    domain: domain.to_string(),
                    raw_domain: format!("raw-{domain}"),
                    routes: vec![PortRoute {
                        https: true,
                        domain: domain.to_string(),
                        host_port: 8080,
                        wildcard: false,
                        container_port: "8080/tcp".to_string(),
                    }],
                    additional_hosts: Vec::new(),
                    status_port: None,
                    config_hash: String::new(),
                    source: None,
                    terminating: false,
                    debug_wait: false,
                },
                observed: Default::default(),
                pending_action: None,
            },
        );
        EnvState {
            network: network.to_string(),
            containers,
            ..Default::default()
        }
    }

    #[test]
    fn the_snapshot_tracks_the_environment() {
        let effect = RouteTableEffect::new();
        let mut state = WorkspaceState::default();
        assert!(effect.extract(&state).is_none());
        state.env = Some(env_with_route("net", "a.test"));
        assert!(effect.extract(&state).is_some());
    }

    #[test]
    fn an_environment_without_a_network_yet_is_skipped() {
        let mut effect = RouteTableEffect::new();
        effect
            .converge(&Some(EnvState::default()))
            .expect("must not fail");
        assert!(effect.written.is_none());
    }
}
