//! Fans every registered workspace's running containers' `#AdditionalHost`
//! aliases into `hosts_file::sync`, using the same `FannedInEffect` idiom
//! as `effects::raw_net::RawNetEffect`. Nothing else may call
//! `hosts_file::sync` while this effect runs — two writers of the same
//! managed `/etc/hosts` block would just race each other.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::effects::FannedInEffect;
use crate::hosts_file;
use crate::state::WorkspaceState;

/// Every `#AdditionalHost` alias currently claimed by a `"running"`
/// container across every workspace in `states`, using the same
/// `observed.status == "running"` filter `effects::raw_net::extract_endpoints`
/// already applies. `hosts_file::sync` itself also sorts/dedupes its input,
/// but doing it here too keeps this effect's `Snapshot` comparison
/// (`PartialEq` on the `Vec`) exact rather than order-sensitive-by-accident.
fn extract_hosts(states: &BTreeMap<String, Arc<WorkspaceState>>) -> Vec<String> {
    let mut hosts: Vec<String> = states
        .values()
        .flat_map(|state| state.runs.values())
        .flat_map(|run| run.containers.values())
        .filter(|container| container.observed.status == "running")
        .flat_map(|container| container.desired.additional_hosts.iter().cloned())
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// The daemon-wide `/etc/hosts` effect: aggregates every registered
/// workspace's running containers' additional-host aliases and rewrites the
/// fghjd-managed block to match.
pub struct HostsEffect;

impl FannedInEffect for HostsEffect {
    type Snapshot = Vec<String>;

    fn extract(&self, states: &BTreeMap<String, Arc<WorkspaceState>>) -> Self::Snapshot {
        extract_hosts(states)
    }

    fn converge(&mut self, snapshot: &Self::Snapshot) -> anyhow::Result<()> {
        hosts_file::sync(&hosts_file::hosts_path(), snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ContainerInfo;
    use crate::state::testing;

    fn container(node_id: &str, status: &str, additional_hosts: &[&str]) -> ContainerInfo {
        let mut c = testing::container(node_id);
        c.desired.running = status == "running";
        c.desired.additional_hosts = additional_hosts.iter().map(|h| h.to_string()).collect();
        c.observed.status = status.to_string();
        c
    }

    fn workspace(containers: Vec<ContainerInfo>) -> Arc<WorkspaceState> {
        Arc::new(testing::workspace(containers))
    }

    #[test]
    fn extracts_additional_hosts_of_running_containers() {
        let states = BTreeMap::from([(
            "ws".to_string(),
            workspace(vec![container("web", "running", &["app.local.aikido.io"])]),
        )]);

        assert_eq!(extract_hosts(&states), vec!["app.local.aikido.io"]);
    }

    #[test]
    fn excludes_containers_that_are_not_running() {
        let states = BTreeMap::from([(
            "ws".to_string(),
            workspace(vec![container("web", "exited", &["app.local.aikido.io"])]),
        )]);

        assert!(extract_hosts(&states).is_empty());
    }

    #[test]
    fn sorts_and_deduplicates_across_workspaces() {
        let states = BTreeMap::from([
            (
                "ws-a".to_string(),
                workspace(vec![container("web", "running", &["b.local"])]),
            ),
            (
                "ws-b".to_string(),
                workspace(vec![container("api", "running", &["a.local", "b.local"])]),
            ),
        ]);

        assert_eq!(extract_hosts(&states), vec!["a.local", "b.local"]);
    }

    #[test]
    fn extracting_from_no_workspaces_yields_no_hosts() {
        assert!(extract_hosts(&BTreeMap::new()).is_empty());
    }
}
