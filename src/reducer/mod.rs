//! The pure state machine. No I/O, never async, never panics on a bad action:
//! a request can be rejected, a stale report is a no-op.

mod env;
mod observation;

use crate::action::{Action, ActionRejected};
use crate::state::WorkspaceState;

pub fn reduce(state: &WorkspaceState, action: Action) -> Result<WorkspaceState, ActionRejected> {
    match action {
        Action::EnvPlanned { .. }
        | Action::NodeStartRequested { .. }
        | Action::NodeStopRequested { .. }
        | Action::NodeDeleteRequested { .. }
        | Action::NodeDebugWaitRequested { .. } => env::reduce(state, action),

        Action::ContainerObserved { .. }
        | Action::ContainerActionSettled { .. }
        | Action::EnvCreateWorking { .. }
        | Action::EnvCreateProgress { .. }
        | Action::EnvCreateSettled { .. }
        | Action::VolumeObserved { .. }
        | Action::ConfigDriftObserved { .. } => observation::reduce(state, action),

        Action::OwnerSet { owner } => {
            let mut next = state.clone();
            next.owner = Some(owner);
            Ok(next)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::WorkspaceOwner;

    fn owner() -> WorkspaceOwner {
        WorkspaceOwner {
            uid: 501,
            gid: 20,
            home: "/Users/dev".into(),
            ssh_auth_sock: None,
        }
    }

    #[test]
    fn owner_set_replaces_the_workspace_owner() {
        let state = WorkspaceState::default();
        let next = reduce(&state, Action::OwnerSet { owner: owner() }).unwrap();
        assert_eq!(next.owner.unwrap().uid, 501);
    }

    #[test]
    fn owner_set_overwrites_a_previously_set_owner() {
        let state = WorkspaceState {
            owner: Some(owner()),
            ..WorkspaceState::default()
        };
        let mut replacement = owner();
        replacement.uid = 999;
        let next = reduce(&state, Action::OwnerSet { owner: replacement }).unwrap();
        assert_eq!(next.owner.unwrap().uid, 999);
    }

    #[test]
    fn dispatch_never_mutates_the_original_state() {
        let state = WorkspaceState::default();
        let _ = reduce(&state, Action::OwnerSet { owner: owner() }).unwrap();
        assert!(state.owner.is_none());
    }
}
