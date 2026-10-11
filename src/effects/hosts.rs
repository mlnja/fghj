//! //! Writes every workspace's `#AdditionalHost` aliases into the managed
//! //! `/etc/hosts` block. The only writer of that block.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::effects::FannedInEffect;
use crate::hosts_file;
use crate::state::WorkspaceState;

/// Sorted and deduped so the snapshot comparison is exact.
fn extract_hosts(states: &BTreeMap<String, Arc<WorkspaceState>>) -> Vec<String> {
    let mut hosts: Vec<String> = states
        .values()
        .flat_map(|state| state.env.iter())
        .flat_map(|run| run.containers.values())
        .filter(|container| container.observed.status == "running")
        .flat_map(|container| container.desired.additional_hosts.iter().cloned())
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

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
