//! Every way a workspace's state can change. Only `reducer::reduce` turns an
//! `Action` into a new state.

use std::collections::BTreeMap;

use crate::persistence::WorkspaceOwner;
use crate::state::{ContainerInfo, EnvCreateError, EnvSpec, EnvState, PendingAction, SyncStatus};

/// Requests (`*Requested`, `EnvPlanned`, `OwnerSet`) can be rejected; reports
/// (`*Observed`, `*Settled`, `EnvCreate*`) never are.
#[derive(Debug, Clone)]
pub enum Action {
    /// Records the intent only; `effects::docker::converge` does the I/O.
    EnvPlanned {
        plan: EnvSpec,
    },
    NodeStartRequested {
        node_id: String,
    },
    NodeStopRequested {
        node_id: String,
    },
    NodeDebugWaitRequested {
        node_id: String,
        wait: bool,
    },
    NodeDeleteRequested {
        node_id: String,
    },

    ContainerObserved {
        node_id: String,
        status: String,
        published_port: Option<u16>,
        /// Every declared port, so a plain TCP service's port is visible too.
        ports: BTreeMap<String, Option<u16>>,
        exit_code: Option<i64>,
        /// Docker didn't answer; every other field is the last reading.
        unseen: bool,
    },
    /// `Ok(None)` is a delete. The only way post-action state reaches the reducer.
    /// `of` is the action this settles, and the report is dropped once the node
    /// has moved on to another (a start called off by a Stop); `None` for a node
    /// another action touched on the way.
    ContainerActionSettled {
        node_id: String,
        of: Option<PendingAction>,
        result: Result<Option<ContainerInfo>, String>,
    },
    /// `Ok` replaces the environment. `Err` clears `pending_create` and adopts the
    /// partial state if containers did start, so they stay visible and routable.
    EnvCreateSettled {
        result: Result<EnvState, EnvCreateError>,
    },
    /// One node up during a create, so a crash mid-create still has it recorded.
    /// Leaves `pending_create` set.
    EnvCreateProgress {
        network: String,
        sidecar_container_name: String,
        sidecar_ip: Option<String>,
        info: ContainerInfo,
    },
    /// The reconciler started the sidecar again after it went missing. Its address
    /// only differs on a network from before subnets were pinned.
    SidecarRestored {
        sidecar_ip: String,
    },
    /// A create is about to work on `node_id` (`Some`), or gave up on it (`None`).
    EnvCreateWorking {
        node_id: String,
        action: Option<PendingAction>,
    },
    VolumeObserved {
        volume_name: String,
        exists: bool,
    },
    ConfigDriftObserved {
        node_id: String,
        drift: SyncStatus,
    },

    OwnerSet {
        owner: WorkspaceOwner,
    },
}

/// Why the reducer refused a request. Reports are never refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionRejected {
    AlreadyInFlight,
    NoEnvironment,
    NodeNotFound,
}

impl std::fmt::Display for ActionRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionRejected::AlreadyInFlight => write!(f, "an action is already in flight"),
            ActionRejected::NoEnvironment => write!(f, "no environment yet"),
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
        assert_ne!(
            ActionRejected::AlreadyInFlight,
            ActionRejected::NoEnvironment
        );
        assert_ne!(ActionRejected::NoEnvironment, ActionRejected::NodeNotFound);
    }

    #[test]
    fn action_rejected_display_is_human_readable() {
        assert_eq!(
            ActionRejected::AlreadyInFlight.to_string(),
            "an action is already in flight"
        );
    }
}
