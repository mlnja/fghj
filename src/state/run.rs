use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::container::ContainerInfo;
use super::volume::VolumeInfo;

/// The caller-supplied scoping for a `RunPlanned` action, deserialized
/// straight off the `POST /runs` HTTP body: which run to (re)create, and
/// optionally that only the nodes reachable from one flow (see
/// `Node::flows`) should be started — e.g. just the checkout flow's
/// services instead of every service fghj knows about.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RunSpec {
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub flow: Option<String>,
}

/// One run's canonical state: every container and volume fghj knows about
/// for it. Keyed by `node_id` / volume name (`BTreeMap`) rather than a
/// `Vec<ContainerInfo>` — a pure reducer dispatches almost every action by
/// `node_id` (`RunNodeStartRequested`, `ContainerObserved`, ...) and needs
/// point lookup on essentially every action, not a linear scan; a map also
/// structurally rules out the two-containers-same-`node_id` state a `Vec`
/// never prevented.
///
/// The single representation of a run in fghj: the reducer owns the
/// authoritative copy, `runs::RunRegistry` keeps a working copy of the same
/// type while it drives Docker, and `persistence` stores it. There is
/// deliberately no second, flatter "wire" or "engine" shape to translate
/// to and from — two structs for one run is what let `desired` and
/// `observed` silently disagree before they were unified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct RunState {
    pub run_id: String,
    pub network: String,
    pub containers: BTreeMap<String, ContainerInfo>,
    pub volumes: BTreeMap<String, VolumeInfo>,
    /// The deterministic name of this run's in-network TLS proxy sidecar
    /// (see `RunRegistry::ensure_sidecar`) — one per run, never shared
    /// across workspaces. Empty for a run persisted before this field
    /// existed, until that run is next started/topped up.
    pub sidecar_container_name: String,
    /// The sidecar's own address on `network` — `None` until
    /// `docker::inspect_network_ip` has actually resolved it (or for a
    /// pre-sidecar persisted run). Cached here rather than re-inspected on
    /// every node start so `start_node` doesn't need a Docker round-trip
    /// just to set every node's `--dns`.
    pub sidecar_ip: Option<String>,
    /// Set by `RunPlanned` to record the caller's still-unfulfilled intent
    /// to (re)create/top-up this run; `effects::docker::converge` picks it
    /// up, does the real Docker work, and clears it via
    /// `Action::RunCreateSettled`. Never serialized: this is internal
    /// convergence bookkeeping, not part of the run's observable state the
    /// API/UI consume (unlike `ContainerInfo::pending_action`, which the UI
    /// does render — a whole-run creation has no equivalent "starting..."
    /// affordance yet).
    #[serde(skip)]
    pub pending_create: Option<RunSpec>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_spec_defaults_both_fields_when_absent() {
        let spec: RunSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(spec.run_id, None);
        assert_eq!(spec.flow, None);
    }

    #[test]
    fn run_spec_parses_both_fields_when_present() {
        let spec: RunSpec =
            serde_json::from_str(r#"{"run_id":"default","flow":"checkout"}"#).unwrap();
        assert_eq!(spec.run_id.as_deref(), Some("default"));
        assert_eq!(spec.flow.as_deref(), Some("checkout"));
    }

    #[test]
    fn fresh_run_state_has_no_containers_or_volumes() {
        let state = RunState {
            run_id: "default".into(),
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
    fn pending_create_is_never_serialized() {
        let state = RunState {
            run_id: "default".into(),
            network: "fghj-net-default".into(),
            containers: BTreeMap::new(),
            volumes: BTreeMap::new(),
            sidecar_container_name: "fghj-sidecar-default".into(),
            sidecar_ip: None,
            pending_create: Some(RunSpec {
                run_id: None,
                flow: None,
            }),
        };
        let json = serde_json::to_value(&state).unwrap();
        assert!(json.get("pending_create").is_none());
    }
}
