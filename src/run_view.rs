//! Translates the new reducer-owned `state::RunState`/`ContainerInfo` shape
//! into the old flat `runs::RunState`/`ContainerInfo` shape the frontend
//! (`ui/src/App.svelte`, `ui/src/lib/Drawer.svelte`) has always consumed.
//!
//! Exists purely as a migration seam: `daemon::get_runs`/`post_runs` cut
//! over to dispatching through the workspace actor (migration phase 5), but
//! the UI was never touched, so its JSON contract can't move either. Every
//! field the UI actually reads (see the phase-5 gap report) is reproduced
//! here field-for-field; anything with no old-shape equivalent (e.g.
//! `state::VolumeInfo`, `state::ContainerObserved::ip`) is simply dropped,
//! not invented a place in the old shape.
//!
//! Builds real `runs::RunState`/`ContainerInfo` values (not a hand-rolled
//! `serde_json::json!` map) so the JSON shape is guaranteed to match
//! whatever the pre-migration handlers already produced — the same
//! `#[derive(Serialize)]` drives both.

use crate::{runs, state};

pub fn legacy_runs(workspace: &state::WorkspaceState) -> Vec<runs::RunState> {
    workspace.runs.values().map(legacy_run).collect()
}

pub fn legacy_run(run: &state::RunState) -> runs::RunState {
    runs::RunState {
        run_id: run.run_id.clone(),
        network: run.network.clone(),
        containers: run.containers.values().map(legacy_container).collect(),
        sidecar_container_name: run.sidecar_container_name.clone(),
        sidecar_ip: run.sidecar_ip.clone(),
    }
}

pub fn legacy_container(container: &state::ContainerInfo) -> runs::ContainerInfo {
    runs::ContainerInfo {
        node_id: container.node_id.clone(),
        container_name: container.desired.container_name.clone(),
        status: container.observed.status.clone(),
        published_port: container.observed.published_port,
        domain: container.desired.domain.clone(),
        raw_domain: container.desired.raw_domain.clone(),
        routes: container.desired.routes.iter().map(legacy_route).collect(),
        additional_hosts: container.desired.additional_hosts.clone(),
        ports: container.observed.ports.clone(),
        status_port: container.desired.status_port.clone(),
        config_hash: container.desired.config_hash.clone(),
        synced: legacy_sync(container.observed.sync),
        pending_action: container.pending_action.map(legacy_pending_action),
    }
}

fn legacy_route(route: &state::PortRoute) -> runs::PortRoute {
    runs::PortRoute {
        domain: route.domain.clone(),
        host_port: route.host_port,
        wildcard: route.wildcard,
        container_port: route.container_port.clone(),
        https: route.https,
    }
}

fn legacy_sync(sync: state::SyncStatus) -> Option<bool> {
    match sync {
        state::SyncStatus::Unknown => None,
        state::SyncStatus::Synced => Some(true),
        state::SyncStatus::Drifted => Some(false),
    }
}

fn legacy_pending_action(pending: state::PendingAction) -> runs::PendingAction {
    match pending {
        state::PendingAction::Starting => runs::PendingAction::Starting,
        state::PendingAction::Stopping => runs::PendingAction::Stopping,
        state::PendingAction::Removing => runs::PendingAction::Removing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn container(node_id: &str) -> state::ContainerInfo {
        state::ContainerInfo {
            node_id: node_id.into(),
            desired: state::ContainerDesired {
                running: true,
                container_name: format!("fghj-{node_id}-1"),
                domain: format!("{node_id}.fghj.internal"),
                raw_domain: format!("{node_id}.fghj.raw.internal"),
                routes: vec![state::PortRoute {
                    domain: format!("{node_id}.fghj.internal"),
                    host_port: 54321,
                    wildcard: false,
                    container_port: "80".into(),
                    https: true,
                }],
                additional_hosts: vec!["alias.example.com".into()],
                status_port: Some("80".into()),
                config_hash: "abc123".into(),
            },
            observed: state::ContainerObserved::default(),
            pending_action: None,
        }
    }

    #[test]
    fn legacy_container_carries_every_field_the_ui_reads_with_no_pending_action() {
        let c = container("web");
        let legacy = legacy_container(&c);
        assert_eq!(legacy.node_id, "web");
        assert_eq!(legacy.container_name, "fghj-web-1");
        assert_eq!(legacy.domain, "web.fghj.internal");
        assert_eq!(legacy.raw_domain, "web.fghj.raw.internal");
        assert_eq!(legacy.status_port.as_deref(), Some("80"));
        assert_eq!(legacy.config_hash, "abc123");
        assert_eq!(legacy.additional_hosts, vec!["alias.example.com"]);
        assert_eq!(legacy.routes.len(), 1);
        assert_eq!(legacy.routes[0].domain, "web.fghj.internal");
        assert_eq!(legacy.routes[0].host_port, 54321);
        assert!(!legacy.routes[0].wildcard);
        assert_eq!(legacy.routes[0].container_port, "80");
        assert!(legacy.routes[0].https);
        assert_eq!(legacy.synced, None);
        assert!(legacy.pending_action.is_none());
    }

    #[test]
    fn legacy_container_carries_a_pending_action() {
        let mut c = container("web");
        c.pending_action = Some(state::PendingAction::Starting);
        let legacy = legacy_container(&c);
        assert_eq!(legacy.pending_action, Some(runs::PendingAction::Starting));
    }

    #[test]
    fn legacy_container_carries_observed_status_ports_and_sync() {
        let mut c = container("web");
        c.observed.status = "running".into();
        c.observed.published_port = Some(12345);
        c.observed.ip = Some("172.20.0.5".into());
        c.observed.ports = BTreeMap::from([("80".to_string(), Some(12345))]);
        c.observed.sync = state::SyncStatus::Drifted;
        let legacy = legacy_container(&c);
        assert_eq!(legacy.status, "running");
        assert_eq!(legacy.published_port, Some(12345));
        assert_eq!(legacy.ports.get("80"), Some(&Some(12345)));
        assert_eq!(legacy.synced, Some(false));
    }

    #[test]
    fn legacy_sync_maps_every_variant() {
        assert_eq!(legacy_sync(state::SyncStatus::Unknown), None);
        assert_eq!(legacy_sync(state::SyncStatus::Synced), Some(true));
        assert_eq!(legacy_sync(state::SyncStatus::Drifted), Some(false));
    }

    #[test]
    fn legacy_run_carries_run_level_fields_and_every_container() {
        let mut run = state::RunState {
            run_id: "default".into(),
            network: "fghj-net-default".into(),
            containers: BTreeMap::new(),
            volumes: BTreeMap::new(),
            sidecar_container_name: "fghj-sidecar".into(),
            sidecar_ip: Some("172.20.0.2".into()),
            pending_create: None,
        };
        run.containers.insert("web".into(), container("web"));
        let legacy = legacy_run(&run);
        assert_eq!(legacy.run_id, "default");
        assert_eq!(legacy.network, "fghj-net-default");
        assert_eq!(legacy.sidecar_container_name, "fghj-sidecar");
        assert_eq!(legacy.sidecar_ip.as_deref(), Some("172.20.0.2"));
        assert_eq!(legacy.containers.len(), 1);
        assert_eq!(legacy.containers[0].node_id, "web");
    }

    #[test]
    fn legacy_runs_lists_every_run_in_the_workspace() {
        let mut workspace = state::WorkspaceState::default();
        workspace.runs.insert(
            "default".into(),
            state::RunState {
                run_id: "default".into(),
                network: "fghj-net-default".into(),
                containers: BTreeMap::new(),
                volumes: BTreeMap::new(),
                sidecar_container_name: "fghj-sidecar".into(),
                sidecar_ip: None,
                pending_create: None,
            },
        );
        let list = legacy_runs(&workspace);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].run_id, "default");
    }
}
