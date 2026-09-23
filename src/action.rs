//! Every way a workspace's `state::WorkspaceState` can legally change —
//! see the architecture plan (rosy-soaring-teapot.md) for the full
//! rationale. `reducer::reduce` is the only code allowed to turn an
//! `Action` into a new `WorkspaceState`; every mutating HTTP handler in
//! later migration phases is expected to do nothing but build one of
//! these and dispatch it (`actor::ActorHandle::dispatch`) rather than
//! mutate anything directly.

use std::collections::BTreeMap;

use crate::persistence::WorkspaceOwner;
use crate::state::{ContainerInfo, RunSpec, RunState, SyncStatus};

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
    /// `plan` is `state::RunSpec`, not the architecture plan's literal
    /// `RunPlan` sketch — see `state::run::RunSpec`'s doc for why. The
    /// `.fghj.yaml` graph is still resolved by the caller *before*
    /// dispatch (real I/O has no place in a pure reducer); this action
    /// only ever expresses the already-resolved intent.
    RunPlanned {
        run_id: String,
        plan: RunSpec,
    },
    RunStopRequested {
        run_id: String,
    },
    RunNodeStartRequested {
        run_id: String,
        node_id: String,
    },
    RunNodeStopRequested {
        run_id: String,
        node_id: String,
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
    },
    /// Reported by `effects::docker::converge` once a start/stop/delete call
    /// it triggered actually finishes. Carries the freshly re-observed
    /// `ContainerInfo` on success (`Ok(Some(..))` for start/stop, `Ok(None)`
    /// for a delete that removed the container outright) rather than just
    /// `Ok(())`, since there's no longer a continuously-polling bridge to
    /// pick up the real post-action status/ports/routes afterwards — this
    /// is now the only place that ever happens.
    ContainerActionSettled {
        run_id: String,
        node_id: String,
        result: Result<Option<ContainerInfo>, String>,
    },
    /// Reported by `effects::docker::converge` once a `RunPlanned` intent
    /// (`RunState::pending_create`) actually finishes being created/topped
    /// up against real Docker. `Ok(run)` wholesale-replaces the run's entry
    /// with the freshly re-observed state (mirroring how `RunRegistry::start`/
    /// `ensure_running` return the whole `RunState` they just produced);
    /// `Err` just clears `pending_create`, leaving whatever was already
    /// there (a fresh empty run, or an existing run's last-known-good state
    /// for a failed top-up) untouched rather than guessing at what's real.
    RunCreateSettled {
        run_id: String,
        result: Result<RunState, String>,
    },
    VolumeObserved {
        run_id: String,
        volume_name: String,
        exists: bool,
    },
    /// Addressed by `run_id`/`node_id`, unlike the architecture plan's
    /// literal `ConfigDriftObserved { drift: SyncStatus }` sketch —
    /// `RunRegistry::config_drift` (which computes the verdicts this
    /// action reports)
    /// computes drift per-container, never once for a whole workspace, so
    /// this needs the same addressing `ContainerObserved` has, to know
    /// which container's `ContainerObserved::sync` to update. This is a
    /// deliberate gap-fill over the plan's literal snippet — see the
    /// phase-1 report for this judgment call.
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
    /// `container.pending_action.is_some()` already — the reducer-level
    /// replacement for today's `RunRegistry::pending`/`PendingGuard`
    /// (`src/runs.rs:711-736`); see `reducer::run`'s module doc.
    AlreadyInFlight,
    /// The action named a `run_id` this workspace doesn't currently have a
    /// `RunState` for.
    RunNotFound,
    /// The action named a `node_id` that isn't a key of the named run's
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
