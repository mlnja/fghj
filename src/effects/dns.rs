//! Fans every registered workspace's running containers' `wildcard_hosts`
//! suffixes into `dns::install_os_resolver_config` — the DNS half of the
//! architecture plan's (rosy-soaring-teapot.md) "dns + hosts_file effects"
//! migration step, following the same `FannedInEffect` idiom
//! `effects::raw_net::RawNetEffect` already established for the raw-net
//! slice. `daemon.rs`'s `spawn_reconciler` must never also call
//! `dns::install_os_resolver_config` directly once this effect is spawned —
//! two writers of the same `/etc/resolver` directory would just race each
//! other to reach the same end state.
//!
//! Only the *write* side of DNS (which zones get routed to this server) is
//! an `Effect` in this sense — actually *answering* a query
//! (`dns::ZoneSource::answer_for`, still implemented by
//! `daemon::WorkspaceRegistry` off the old system) is a live read path, not
//! a converge-reality-to-state side effect, so it's out of scope here and
//! migrates only once the old `WorkspaceRegistry`/`RunRegistry` itself
//! retires (migration phase 5+).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::dns;
use crate::effects::FannedInEffect;
use crate::state::WorkspaceState;

/// Every `wildcard_hosts` suffix currently claimed by a `"running"`
/// container across every workspace in `states`, using the same
/// `observed.status == "running"` filter `effects::raw_net::extract_endpoints`
/// already applies. Sorted and deduplicated for the same reason
/// `effects::hosts::extract_hosts` is: an exact, order-independent
/// `Snapshot` comparison.
fn extract_wildcard_suffixes(states: &BTreeMap<String, Arc<WorkspaceState>>) -> Vec<String> {
    let mut zones: Vec<String> = states
        .values()
        .flat_map(|state| state.runs.values())
        .flat_map(|run| run.containers.values())
        .filter(|container| container.observed.status == "running")
        .flat_map(|container| {
            container
                .desired
                .routes
                .iter()
                .filter(|route| route.wildcard)
                .map(|route| route.domain.clone())
        })
        .collect();
    zones.sort();
    zones.dedup();
    zones
}

/// The daemon-wide DNS-routing effect: aggregates every registered
/// workspace's running containers' wildcard-host suffixes and syncs the OS
/// resolver config (macOS `/etc/resolver`) so each one routes to this DNS
/// server at `port`.
pub struct DnsEffect {
    pub port: u16,
}

impl FannedInEffect for DnsEffect {
    type Snapshot = Vec<String>;

    fn extract(&self, states: &BTreeMap<String, Arc<WorkspaceState>>) -> Self::Snapshot {
        extract_wildcard_suffixes(states)
    }

    fn converge(&mut self, snapshot: &Self::Snapshot) -> anyhow::Result<()> {
        dns::install_os_resolver_config(self.port, snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        ContainerDesired, ContainerInfo, ContainerObserved, PortRoute, RunState, SyncStatus,
    };

    fn route(domain: &str, wildcard: bool) -> PortRoute {
        PortRoute {
            domain: domain.to_string(),
            host_port: 54321,
            wildcard,
            container_port: "80".to_string(),
            https: true,
        }
    }

    fn container(node_id: &str, status: &str, routes: Vec<PortRoute>) -> ContainerInfo {
        ContainerInfo {
            node_id: node_id.to_string(),
            desired: ContainerDesired {
                running: status == "running",
                container_name: format!("fghj-{node_id}-1"),
                domain: format!("{node_id}.fghj.internal"),
                raw_domain: format!("{node_id}.fghj.raw.internal"),
                routes,
                additional_hosts: vec![],
                status_port: None,
                config_hash: "hash".into(),
            },
            observed: ContainerObserved {
                status: status.to_string(),
                published_port: None,
                ip: None,
                ports: BTreeMap::new(),
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
    fn extracts_wildcard_suffixes_of_running_containers() {
        let states = BTreeMap::from([(
            "ws".to_string(),
            workspace_with(BTreeMap::from([(
                "default".to_string(),
                run_state(
                    "default",
                    vec![container(
                        "web",
                        "running",
                        vec![route("myservice.local", true)],
                    )],
                ),
            )])),
        )]);

        assert_eq!(extract_wildcard_suffixes(&states), vec!["myservice.local"]);
    }

    #[test]
    fn excludes_non_wildcard_routes() {
        let states = BTreeMap::from([(
            "ws".to_string(),
            workspace_with(BTreeMap::from([(
                "default".to_string(),
                run_state(
                    "default",
                    vec![container(
                        "web",
                        "running",
                        vec![route("web.fghj.internal", false)],
                    )],
                ),
            )])),
        )]);

        assert!(extract_wildcard_suffixes(&states).is_empty());
    }

    #[test]
    fn excludes_containers_that_are_not_running() {
        let states = BTreeMap::from([(
            "ws".to_string(),
            workspace_with(BTreeMap::from([(
                "default".to_string(),
                run_state(
                    "default",
                    vec![container(
                        "web",
                        "exited",
                        vec![route("myservice.local", true)],
                    )],
                ),
            )])),
        )]);

        assert!(extract_wildcard_suffixes(&states).is_empty());
    }

    #[test]
    fn sorts_and_deduplicates_across_workspaces() {
        let states = BTreeMap::from([
            (
                "ws-a".to_string(),
                workspace_with(BTreeMap::from([(
                    "default".to_string(),
                    run_state(
                        "default",
                        vec![container("web", "running", vec![route("b.local", true)])],
                    ),
                )])),
            ),
            (
                "ws-b".to_string(),
                workspace_with(BTreeMap::from([(
                    "default".to_string(),
                    run_state(
                        "default",
                        vec![container(
                            "api",
                            "running",
                            vec![route("a.local", true), route("b.local", true)],
                        )],
                    ),
                )])),
            ),
        ]);

        assert_eq!(
            extract_wildcard_suffixes(&states),
            vec!["a.local", "b.local"]
        );
    }

    #[test]
    fn extracting_from_no_workspaces_yields_no_zones() {
        assert!(extract_wildcard_suffixes(&BTreeMap::new()).is_empty());
    }
}
