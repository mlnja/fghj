//! Renders every run's sidecar route table from workspace state.
//!
//! Each run's sidecar proxy polls a `routes.json` naming every domain it
//! should answer for and where to connect. That file is a pure function of
//! `RunState::containers`, so it belongs here rather than being written by
//! hand at the end of each lifecycle call — which is what `runs/` used to
//! do, from a `RunState` snapshot taken *before* the Docker work, and
//! therefore occasionally from a snapshot that had already lost a sibling
//! node's routes.
//!
//! Driving it off published state instead removes that whole class of
//! mistake: the table is rewritten whenever the state it is derived from
//! changes, from whatever the state actually says at that moment.

use std::collections::BTreeMap;

use crate::runs::route_table::{sidecar_routes_dir, write_route_table};
use crate::state::{RunState, WorkspaceState};

use super::Effect;

/// Keyed by run id so a removed run's directory can be cleaned up, and
/// carrying the whole `RunState` because `write_route_table` derives the
/// network path and the entries from it together.
pub type Snapshot = BTreeMap<String, RunState>;

#[derive(Default)]
pub struct RouteTableEffect {
    /// Networks whose directories exist because this effect wrote them.
    /// Kept so a run that disappears has its table removed rather than
    /// left behind for a sidecar to keep serving.
    written: BTreeMap<String, String>,
}

impl RouteTableEffect {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Effect for RouteTableEffect {
    type Snapshot = Snapshot;

    fn extract(&self, state: &WorkspaceState) -> Self::Snapshot {
        state.runs.clone()
    }

    fn converge(&mut self, snapshot: &Self::Snapshot) -> anyhow::Result<()> {
        for (run_id, run) in snapshot {
            if run.network.is_empty() {
                // A run that has been planned but not yet created has no
                // network to write a table into. Nothing to do until it
                // does.
                continue;
            }
            write_route_table(run)?;
            self.written.insert(run_id.clone(), run.network.clone());
        }
        // A run that is gone from state takes its route table with it.
        // Leaving the file would let a sidecar keep answering for domains
        // that no longer resolve to anything.
        self.written.retain(|run_id, network| {
            if snapshot.contains_key(run_id) {
                return true;
            }
            let _ = std::fs::remove_dir_all(sidecar_routes_dir(network));
            false
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ContainerDesired, ContainerInfo, PortRoute};

    fn run_with_route(run_id: &str, network: &str, domain: &str) -> RunState {
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
                },
                observed: Default::default(),
                pending_action: None,
            },
        );
        RunState {
            run_id: run_id.to_string(),
            network: network.to_string(),
            containers,
            ..Default::default()
        }
    }

    /// The snapshot is the run map itself, so any change to any run's
    /// containers re-triggers a write — that is what makes this effect a
    /// replacement for the hand-placed calls it removes.
    #[test]
    fn the_snapshot_tracks_the_runs_map() {
        let effect = RouteTableEffect::new();
        let mut state = WorkspaceState::default();
        assert!(effect.extract(&state).is_empty());
        state
            .runs
            .insert("default".into(), run_with_route("default", "net", "a.test"));
        assert_eq!(effect.extract(&state).len(), 1);
    }

    /// A planned-but-not-yet-created run has no network, so there is no
    /// directory to write into. It must be skipped rather than producing a
    /// path like `.../runs//routes.json`.
    #[test]
    fn a_run_without_a_network_yet_is_skipped() {
        let mut effect = RouteTableEffect::new();
        let mut snapshot = Snapshot::new();
        snapshot.insert(
            "default".into(),
            RunState {
                run_id: "default".into(),
                ..Default::default()
            },
        );
        effect.converge(&snapshot).expect("must not fail");
        assert!(effect.written.is_empty());
    }
}
