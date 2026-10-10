use std::collections::BTreeMap;

use serde::Serialize;

use super::container::ContainerInfo;
use super::volume::VolumeInfo;

/// Why the environment is being brought up to date — the intent a
/// `RunPlanned` action records and `effects::docker::converge` fulfils.
/// Serialized as `{"flow": ...}` / `{"node": ...}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunSpec {
    /// Switch to a flow: start its nodes and stop (not remove) every
    /// running container outside it, so the environment ends up as that
    /// flow and nothing else.
    Flow(String),
    /// Start one node, and what it can't start without, when the
    /// environment doesn't exist yet.
    Node(String),
}

/// One run's canonical state: every container and volume fghj knows about
/// for it. Keyed by `node_id` / volume name (`BTreeMap`) rather than a
/// `Vec<ContainerInfo>` — a pure reducer dispatches almost every action by
/// `node_id` (`RunNodeStartRequested`, `ContainerObserved`, ...) and needs
/// point lookup on essentially every action, not a linear scan; a map also
/// structurally rules out the two-containers-same-`node_id` state a `Vec`
/// never prevented.
///
/// The single representation of the environment in fghj, held in a single
/// place: the actor's `WorkspaceState`. `runs::RunRegistry` keeps no copy;
/// it takes whatever prior state a call needs as an argument and reports
/// the result back as an `Action`. `persistence` stores it, derived from
/// published state by `effects::persist`.
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
    /// `Action::RunCreateSettled`. Serialized because it is observable: while
    /// it is set every node start is rejected as already in flight, and the
    /// UI says why ("switching to flow …") instead of leaving the user to
    /// guess.
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
/// The `partial` field exists because `ensure_running` brings nodes up one
/// at a time, and containers 1 and 2 coming up is a real, wanted outcome
/// that a failure on container 3 must not undo; it persists after every
/// node precisely so that progress survives.
///
/// The reducer has to hear about that progress too: a bare `Err(String)`
/// would leave those containers running, persisted to SQLite and the
/// sidecar route table, yet absent from `GET /environment`, the UI and host
/// routing (`state::query::resolve_route` reads reducer state only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCreateError {
    pub message: String,
    /// What is actually up now. `None` when nothing came up.
    ///
    /// Boxed because this is the `Err` half of a `Result` that the whole
    /// create path returns: a `RunState` inline makes every such `Result`
    /// (including every success) at least 224 bytes wide, which is what
    /// `clippy::result_large_err` objects to. A partial run is the rare case,
    /// so one allocation on the failure path is the right trade.
    pub partial: Option<Box<RunState>>,
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
    fn pending_create_serializes_as_the_intent() {
        let state = RunState {
            run_id: "default".into(),
            network: "fghj-net-default".into(),
            containers: BTreeMap::new(),
            volumes: BTreeMap::new(),
            sidecar_container_name: "fghj-sidecar-default".into(),
            sidecar_ip: None,
            pending_create: Some(RunSpec::Node("web".into())),
            pending_teardown: false,
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["pending_create"], serde_json::json!({ "node": "web" }));
    }
}
