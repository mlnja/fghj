//! Every way a workspace's `state::WorkspaceState` can legally change.
//! `reducer::reduce` is the only code allowed to turn an `Action` into a
//! new `WorkspaceState`; a mutating HTTP handler does nothing but build one
//! of these and dispatch it (`actor::ActorHandle::dispatch`).

use std::collections::BTreeMap;

use crate::persistence::WorkspaceOwner;
use crate::state::{ContainerInfo, PendingAction, RunCreateError, RunSpec, RunState, SyncStatus};

/// Split into two families by who originates them: `Run*`/`OwnerSet` are
/// *requests* — dispatched by an HTTP handler (or, later, an internal
/// scheduler) expressing what should become true; `*Observed`/
/// `ContainerActionSettled` are *reports* — dispatched by a polling or
/// convergence effect describing what already *is* true. A reducer never
/// rejects a report for a reason a caller could act on (see
/// `reducer::observation`'s module doc) — only a request can be rejected.
///
/// Does not derive `PartialEq`: `OwnerSet` carries a `WorkspaceOwner`
/// (`persistence::workspace_owner`), which doesn't derive it either. Tests that need to assert
/// on a specific variant destructure/match it instead of comparing whole
/// `Action` values.
#[derive(Debug, Clone)]
pub enum Action {
    /// Records the intent only; the `.fghj.yaml` graph is resolved later
    /// by `effects::docker::converge`, since real I/O has no place in a
    /// pure reducer.
    RunPlanned {
        run_id: String,
        plan: RunSpec,
    },
    RunNodeStartRequested {
        run_id: String,
        node_id: String,
    },
    RunNodeStopRequested {
        run_id: String,
        node_id: String,
    },
    /// Flip the per-container debug switch and recreate the container to
    /// apply it — see `state::ContainerDesired::debug_wait`.
    RunNodeDebugWaitRequested {
        run_id: String,
        node_id: String,
        wait: bool,
    },
    RunNodeDeleteRequested {
        run_id: String,
        node_id: String,
    },

    ContainerObserved {
        run_id: String,
        node_id: String,
        status: String,
        published_port: Option<u16>,
        ip: Option<String>,
        /// The host-published port for every one of the container's
        /// declared ports, not just the routed/primary one — mirrors
        /// `state::ContainerObserved::ports`, since a plain TCP dependency
        /// (postgres, mysql) with no HTTP surface still needs its port
        /// drift visible even though it has no `status_port` of its own.
        ports: BTreeMap<String, Option<u16>>,
        /// What the container exited with, if it has. Only ever the
        /// *outcome* for a terminating node (see
        /// `state::ContainerDesired::terminating`) — for a service it is one
        /// more detail of a crash — but it is observed the same way for
        /// both, because nothing doing the observing holds the graph that
        /// would say which kind this is.
        exit_code: Option<i64>,
    },
    /// Reported by `effects::docker::converge` once a start/stop/delete call
    /// it triggered actually finishes. Carries the freshly re-observed
    /// `ContainerInfo` on success (`Ok(Some(..))` for start/stop, `Ok(None)`
    /// for a delete that removed the container outright) rather than just
    /// `Ok(())`: this is the only place the real post-action
    /// status/ports/routes reach state.
    ContainerActionSettled {
        run_id: String,
        node_id: String,
        result: Result<Option<ContainerInfo>, String>,
    },
    /// Reported by `effects::docker::converge` once a `RunPlanned` intent
    /// (`RunState::pending_create`) actually finishes being created/topped
    /// up against real Docker. `Ok(run)` wholesale-replaces the run's entry
    /// with the freshly re-observed state (mirroring how
    /// `RunRegistry::ensure_running` returns the whole `RunState` it produced).
    /// `Err` clears `pending_create` and, when the failure left containers
    /// actually running (`RunCreateError::partial` — an `ensure_running`
    /// top-up that got part way), replaces the run's entry with that partial
    /// state rather than guessing. Without it, those containers are running
    /// and persisted but invisible to `GET /environment` and to host routing. With
    /// no partial (nothing started, or a path like `start` that rolled
    /// itself back) the run's last-known-good state is left untouched, which
    /// is still the right answer.
    RunCreateSettled {
        run_id: String,
        result: Result<RunState, RunCreateError>,
    },
    /// Reported by `effects::docker::converge` for each node that comes up
    /// during a still-in-flight create/top-up, so progress is recorded as it
    /// happens rather than only when the whole run settles.
    ///
    /// Persistence is derived from published state
    /// (`effects::persist`), so without this a daemon that died mid-create
    /// would leave running containers nothing had recorded — fghj
    /// manufacturing the very `Orphaned` state its observer exists to
    /// surface. Carries the run-level facts (`network`, sidecar) alongside
    /// the container because they are only known once the run's network and
    /// sidecar exist, which is also when the first node can come up.
    ///
    /// Deliberately leaves `pending_create` set: the run is still in
    /// flight, and only `RunCreateSettled` ends that.
    RunCreateProgress {
        run_id: String,
        network: String,
        sidecar_container_name: String,
        sidecar_ip: Option<String>,
        info: ContainerInfo,
    },
    /// Reported by `effects::docker::converge` as a still-in-flight
    /// create/top-up/switch is about to start or stop `node_id` (`Some`), or
    /// has given up on it (`None`) — so the node carries the same
    /// `pending_action` mark a single-node start or stop would, rather than
    /// sitting unchanged while the environment rejects every node action as
    /// already in flight. `RunCreateProgress` and `RunCreateSettled` take the
    /// mark back off.
    RunCreateWorking {
        run_id: String,
        node_id: String,
        action: Option<PendingAction>,
    },
    VolumeObserved {
        run_id: String,
        volume_name: String,
        exists: bool,
    },
    /// Addressed per container, like `ContainerObserved`:
    /// `RunRegistry::config_drift` computes drift per container, and this
    /// updates that container's `ContainerObserved::sync`.
    ConfigDriftObserved {
        run_id: String,
        node_id: String,
        drift: SyncStatus,
    },

    OwnerSet {
        owner: WorkspaceOwner,
    },
}

/// Why the reducer refused to apply a requested `Action` — never returned
/// for a report (`*Observed`/`ContainerActionSettled`; see `Action`'s doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionRejected {
    /// `container.pending_action.is_some()` already, or a whole-environment
    /// create is still running; see `reducer::run`'s module doc.
    AlreadyInFlight,
    /// The action named a `run_id` this workspace doesn't currently have a
    /// `RunState` for.
    RunNotFound,
    /// The action named a `node_id` that isn't a key of the run's
    /// `containers` map.
    NodeNotFound,
}

impl std::fmt::Display for ActionRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionRejected::AlreadyInFlight => write!(f, "an action is already in flight"),
            ActionRejected::RunNotFound => write!(f, "no such run"),
            ActionRejected::NodeNotFound => write!(f, "no such node"),
        }
    }
}

impl std::error::Error for ActionRejected {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_rejected_variants_are_distinguishable() {
        assert_ne!(ActionRejected::AlreadyInFlight, ActionRejected::RunNotFound);
        assert_ne!(ActionRejected::RunNotFound, ActionRejected::NodeNotFound);
    }

    #[test]
    fn action_rejected_display_is_human_readable() {
        assert_eq!(
            ActionRejected::AlreadyInFlight.to_string(),
            "an action is already in flight"
        );
    }
}
