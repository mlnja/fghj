//! Fans every registered workspace's raw-zone containers into the existing,
//! already-working `raw_net::reconcile` — the architecture plan's
//! (rosy-soaring-teapot.md) "First real slice: raw-net" migration step, and
//! the first concrete `FannedInEffect`. `daemon.rs`'s `spawn_reconciler`
//! must never also call `raw_net::reconcile` directly once this is spawned —
//! two schedules touching the same `pf` state is the exact bug class
//! documented in `raw_net::macos`'s module doc.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::effects::FannedInEffect;
use crate::raw_net::{self, RawEndpoint};
use crate::state::WorkspaceState;

/// One raw-zone container's identity and published ports, projected out of
/// a workspace's `state::WorkspaceState` — a local stand-in for
/// `raw_net::RawEndpoint` (which derives neither `PartialEq` nor `Clone`,
/// so can't be used directly as a `FannedInEffect::Snapshot`) that
/// `converge` converts to the real type right before calling `reconcile`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointSnapshot {
    raw_domain: String,
    ports: BTreeMap<String, u16>,
}

/// Extracts one `EndpointSnapshot` per currently-`"running"` container
/// across every workspace in `states`, using only its published (`Some`)
/// ports — the same filter `daemon::WorkspaceRegistry::active_raw_endpoints`
/// (the path this effect replaces) already applied. Pure, so it's testable
/// against hand-built `WorkspaceState`s without a real `ActorRegistry`.
fn extract_endpoints(states: &BTreeMap<String, Arc<WorkspaceState>>) -> Vec<EndpointSnapshot> {
    states
        .values()
        .flat_map(|state| state.runs.values())
        .flat_map(|run| run.containers.values())
        .filter(|container| container.observed.status == "running")
        .map(|container| EndpointSnapshot {
            raw_domain: container.desired.raw_domain.clone(),
            ports: container
                .observed
                .ports
                .iter()
                .filter_map(|(port, host_port)| host_port.map(|p| (port.clone(), p)))
                .collect(),
        })
        .collect()
}

/// The daemon-wide raw-net effect: aggregates every registered workspace's
/// running containers and reconciles the pf/virtual-IP NAT to match.
pub struct RawNetEffect;

impl FannedInEffect for RawNetEffect {
    type Snapshot = Vec<EndpointSnapshot>;

    fn extract(&self, states: &BTreeMap<String, Arc<WorkspaceState>>) -> Self::Snapshot {
        extract_endpoints(states)
    }

    fn converge(&mut self, snapshot: &Self::Snapshot) -> anyhow::Result<()> {
        let endpoints: Vec<RawEndpoint> = snapshot
            .iter()
            .map(|e| RawEndpoint {
                raw_domain: e.raw_domain.clone(),
                ports: e.ports.clone(),
            })
            .collect();
        raw_net::reconcile(&endpoints)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ContainerDesired, ContainerInfo, ContainerObserved, RunState, SyncStatus};

    fn container(node_id: &str, status: &str, ports: &[(&str, Option<u16>)]) -> ContainerInfo {
        ContainerInfo {
            node_id: node_id.to_string(),
            desired: ContainerDesired {
                running: status == "running",
                container_name: format!("fghj-{node_id}-1"),
                domain: format!("{node_id}.fghj.internal"),
                raw_domain: format!("{node_id}.fghj.raw.internal"),
                routes: vec![],
                additional_hosts: vec![],
                status_port: None,
                config_hash: "hash".into(),
            },
            observed: ContainerObserved {
                status: status.to_string(),
                published_port: None,
                ip: None,
                ports: ports.iter().map(|(p, hp)| (p.to_string(), *hp)).collect(),
                sync: SyncStatus::Unknown,
            },
            pending_action: None,
        }
    }

    fn workspace_with(runs: BTreeMap<String, RunState>) -> Arc<WorkspaceState> {
        Arc::new(WorkspaceState {
            path: Default::default(),
            owner: None,
            runs,
        })
    }

    fn run_state(run_id: &str, containers: Vec<ContainerInfo>) -> RunState {
        RunState {
            run_id: run_id.to_string(),
            network: "fghj-net".into(),
            containers: containers
                .into_iter()
                .map(|c| (c.node_id.clone(), c))
                .collect(),
            volumes: BTreeMap::new(),
            sidecar_container_name: "fghj-sidecar".into(),
            sidecar_ip: None,
            pending_create: None,
        }
    }

    #[test]
    fn extracts_one_endpoint_per_running_container() {
        let states = BTreeMap::from([(
            "ws".to_string(),
            workspace_with(BTreeMap::from([(
                "default".to_string(),
                run_state(
                    "default",
                    vec![container("web", "running", &[("80", Some(54321))])],
                ),
            )])),
        )]);

        let endpoints = extract_endpoints(&states);
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].raw_domain, "web.fghj.raw.internal");
        assert_eq!(endpoints[0].ports["80"], 54321);
    }

    #[test]
    fn excludes_containers_that_are_not_running() {
        let states = BTreeMap::from([(
            "ws".to_string(),
            workspace_with(BTreeMap::from([(
                "default".to_string(),
                run_state(
                    "default",
                    vec![container("web", "exited", &[("80", Some(54321))])],
                ),
            )])),
        )]);

        assert!(extract_endpoints(&states).is_empty());
    }

    #[test]
    fn drops_ports_docker_has_not_actually_published_yet() {
        let states = BTreeMap::from([(
            "ws".to_string(),
            workspace_with(BTreeMap::from([(
                "default".to_string(),
                run_state(
                    "default",
                    vec![container(
                        "web",
                        "running",
                        &[("80", Some(54321)), ("443", None)],
                    )],
                ),
            )])),
        )]);

        let endpoints = extract_endpoints(&states);
        assert_eq!(endpoints[0].ports.len(), 1);
        assert!(endpoints[0].ports.contains_key("80"));
    }

    #[test]
    fn a_running_container_with_no_published_ports_still_yields_an_endpoint() {
        let states = BTreeMap::from([(
            "ws".to_string(),
            workspace_with(BTreeMap::from([(
                "default".to_string(),
                run_state("default", vec![container("web", "running", &[])]),
            )])),
        )]);

        let endpoints = extract_endpoints(&states);
        assert_eq!(endpoints.len(), 1);
        assert!(endpoints[0].ports.is_empty());
    }

    #[test]
    fn aggregates_across_multiple_workspaces_and_runs() {
        let states = BTreeMap::from([
            (
                "ws-a".to_string(),
                workspace_with(BTreeMap::from([(
                    "default".to_string(),
                    run_state(
                        "default",
                        vec![container("web", "running", &[("80", Some(1111))])],
                    ),
                )])),
            ),
            (
                "ws-b".to_string(),
                workspace_with(BTreeMap::from([(
                    "default".to_string(),
                    run_state(
                        "default",
                        vec![container("api", "running", &[("80", Some(2222))])],
                    ),
                )])),
            ),
        ]);

        let endpoints = extract_endpoints(&states);
        assert_eq!(endpoints.len(), 2);
    }

    #[test]
    fn extracting_from_no_workspaces_yields_no_endpoints() {
        assert!(extract_endpoints(&BTreeMap::new()).is_empty());
    }
}
