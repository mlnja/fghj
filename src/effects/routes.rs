//! Renders the environment's sidecar route table from workspace state.
//!
//! The sidecar proxy polls a `routes.json` naming every domain it
//! should answer for and where to connect. That file is a pure function of
//! `EnvState::containers`, so it belongs here rather than being written by
//! hand at the end of each lifecycle call — which is what `runs/` used to
//! do, from a `EnvState` snapshot taken *before* the Docker work, and
//! therefore occasionally from a snapshot that had already lost a sibling
//! node's routes.
//!
//! Driving it off published state instead removes that whole class of
//! mistake: the table is rewritten whenever the state it is derived from
//! changes, from whatever the state actually says at that moment.

use crate::runs::route_table::{sidecar_routes_dir, write_route_table};
use crate::state::{EnvState, WorkspaceState};

use super::Effect;

/// The whole `EnvState`, because `write_route_table` derives the network
/// path and the entries from it together.
pub type Snapshot = Option<EnvState>;

#[derive(Default)]
pub struct RouteTableEffect {
    /// The network whose directory exists because this effect wrote it.
    /// Kept so an environment that disappears has its table removed rather
    /// than left behind for a sidecar to keep serving.
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
        // An environment that has been planned but not yet created has no
        // network to write a table into. Nothing to do until it does.
        let network = snapshot
            .as_ref()
            .filter(|env| !env.network.is_empty())
            .map(|env| env.network.clone());
        if let Some(env) = snapshot.as_ref().filter(|_| network.is_some()) {
            write_route_table(env)?;
        }
        // An environment that is gone from state takes its route table with
        // it. Leaving the file would let a sidecar keep answering for
        // domains that no longer resolve to anything.
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

    /// The snapshot is the environment itself, so any change to its
    /// containers re-triggers a write — that is what makes this effect a
    /// replacement for the hand-placed calls it removes.
    #[test]
    fn the_snapshot_tracks_the_environment() {
        let effect = RouteTableEffect::new();
        let mut state = WorkspaceState::default();
        assert!(effect.extract(&state).is_none());
        state.env = Some(env_with_route("net", "a.test"));
        assert!(effect.extract(&state).is_some());
    }

    /// A planned-but-not-yet-created environment has no network, so there
    /// is no directory to write into. It must be skipped rather than
    /// producing a path like `.../runs//routes.json`.
    #[test]
    fn an_environment_without_a_network_yet_is_skipped() {
        let mut effect = RouteTableEffect::new();
        effect
            .converge(&Some(EnvState::default()))
            .expect("must not fail");
        assert!(effect.written.is_none());
    }
}
