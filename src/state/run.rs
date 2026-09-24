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
/// The single representation of a run in fghj, and — since migration
/// phase 5 — held in a single place: the actor's `WorkspaceState`.
/// `runs::RunRegistry` used to keep a working copy of the same type while
/// it drove Docker; it now takes whatever prior state a call needs as an
/// argument and reports the result back as an `Action`. `persistence`
/// stores it, derived from published state by `effects::persist`.
///
/// There is deliberately no second, flatter "wire" or "engine" shape to
/// translate to and from either — two structs for one run is what let
/// `desired` and `observed` silently disagree before they were unified.
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
    /// Set by `RunStopRequested` to record an unfulfilled intent to tear
    /// this whole run down — containers, network, sidecar, and (for a named
    /// run) its volumes. `effects::docker::converge` picks it up and clears
    /// it by way of `Action::RunTeardownSettled`, whose success arm drops
    /// the run from state entirely.
    ///
    /// Whole-run teardown needs its own flag because it is not expressible
    /// as per-container `pending_action`s: the network, the sidecar and the
    /// volumes belong to the run, not to any node, and marking every
    /// container `Stopping` would leave all three orphaned.
    #[serde(skip)]
    pub pending_teardown: bool,
}

/// Why a whole-run create/top-up failed, and what came up anyway.
///
/// The `partial` field exists because the two create paths fail in opposite
/// ways and both are correct for what they do. `RunRegistry::start` builds a
/// *named* run from nothing, so a failure halfway leaves a half-run nobody
/// asked for and it rolls the whole thing back — `partial` is `None`.
/// `ensure_running` tops up the one shared default environment, where
/// containers 1 and 2 coming up is a real, wanted outcome that a failure on
/// container 3 must not undo; it persists after every node precisely so that
/// progress survives.
///
/// What used to be missing is that the *reducer* never heard about that
/// progress: a bare `Err(String)` cleared `pending_create` and nothing else,
/// so those containers were running, persisted to SQLite and the sidecar
/// route table, and simultaneously absent from `GET /runs`, from the UI, and
/// from host routing (`state::query::resolve_route` reads reducer state
/// only). Carrying the partial state is what keeps the stores of truth from
/// disagreeing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCreateError {
    pub message: String,
    /// What is actually up now. `None` when nothing came up, or when the
    /// path that failed rolled back.
    pub partial: Option<RunState>,
}

impl RunCreateError {
    /// The common case: something went wrong before any node started, or in
    /// a path that cleans up after itself.
    pub fn bare(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            partial: None,
        }
    }
}

impl From<anyhow::Error> for RunCreateError {
    fn from(e: anyhow::Error) -> Self {
        Self::bare(format!("{e:#}"))
    }
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
            pending_teardown: false,
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
            pending_teardown: false,
        };
        let json = serde_json::to_value(&state).unwrap();
        assert!(json.get("pending_create").is_none());
    }
}
