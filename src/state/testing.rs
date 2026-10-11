use super::{ContainerDesired, ContainerInfo, EnvState, WorkspaceState};

pub fn container(node_id: &str) -> ContainerInfo {
    ContainerInfo {
        node_id: node_id.into(),
        desired: ContainerDesired {
            running: true,
            container_name: format!("fghj-{node_id}-1"),
            domain: format!("{node_id}.fghj.internal"),
            raw_domain: format!("{node_id}.fghj.raw.internal"),
            config_hash: "hash".into(),
            ..Default::default()
        },
        observed: Default::default(),
        pending_action: None,
    }
}

pub fn run_state(containers: Vec<ContainerInfo>) -> EnvState {
    EnvState {
        network: "fghj-net".into(),
        containers: containers
            .into_iter()
            .map(|c| (c.node_id.clone(), c))
            .collect(),
        sidecar_container_name: "fghj-sidecar".into(),
        ..Default::default()
    }
}

pub fn workspace(containers: Vec<ContainerInfo>) -> WorkspaceState {
    WorkspaceState {
        env: Some(run_state(containers)),
        ..WorkspaceState::default()
    }
}
