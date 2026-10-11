use std::collections::BTreeMap;

use serde::Serialize;

use super::container::ContainerInfo;
use super::volume::VolumeInfo;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvSpec {
    /// Start the flow's nodes and stop (not remove) everything outside it.
    Flow(String),
    /// Start one node and its requirements when there's no environment yet.
    Node(String),
}

/// The environment: every container and volume fghj knows about. Lives only in
/// the actor's `WorkspaceState`; `Orchestrator` keeps no copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct EnvState {
    pub network: String,
    pub containers: BTreeMap<String, ContainerInfo>,
    pub volumes: BTreeMap<String, VolumeInfo>,
    pub sidecar_container_name: String,
    /// Cached so a node start needs no inspect to set its `--dns`.
    pub sidecar_ip: Option<String>,
    /// The create in flight. Serialized so the UI can say why node actions are
    /// being refused.
    pub pending_create: Option<EnvSpec>,
}

/// Why a create failed, and what came up anyway: earlier nodes stay up, and
/// the reducer must see them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvCreateError {
    pub message: String,
    /// Boxed to keep the `Result` small (`clippy::result_large_err`).
    pub partial: Option<Box<EnvState>>,
}

impl EnvCreateError {
    pub fn bare(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            partial: None,
        }
    }
}

impl From<anyhow::Error> for EnvCreateError {
    fn from(e: anyhow::Error) -> Self {
        Self::bare(format!("{e:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_run_state_has_no_containers_or_volumes() {
        let state = EnvState {
            network: "fghj-net-default".into(),
            containers: BTreeMap::new(),
            volumes: BTreeMap::new(),
            sidecar_container_name: "fghj-sidecar-default".into(),
            sidecar_ip: None,
            pending_create: None,
        };
        assert!(state.containers.is_empty());
        assert!(state.volumes.is_empty());
    }

    #[test]
    fn pending_create_serializes_as_the_intent() {
        let state = EnvState {
            network: "fghj-net-default".into(),
            containers: BTreeMap::new(),
            volumes: BTreeMap::new(),
            sidecar_container_name: "fghj-sidecar-default".into(),
            sidecar_ip: None,
            pending_create: Some(EnvSpec::Node("web".into())),
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["pending_create"], serde_json::json!({ "node": "web" }));
    }
}
