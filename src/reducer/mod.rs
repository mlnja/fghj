//! The pure core of the Redux-style migration (see the architecture plan,
//! rosy-soaring-teapot.md): `reduce` is the only function allowed to turn
//! an `Action` into a new `state::WorkspaceState`. It does no I/O, is
//! never `async`, and never panics on a malformed `Action` — every lookup
//! that might not find its target either returns `Err(ActionRejected)`
//! (for a request; see `run`) or is treated as a stale no-op (for a
//! report; see `observation`). This is what lets `actor::spawn` call it
//! inline on a single-threaded per-workspace loop without ever blocking.

mod observation;
mod run;

use crate::action::{Action, ActionRejected};
use crate::state::WorkspaceState;

/// Applies `action` to `state`, returning the new state on success. Never
/// mutates `state` in place — callers (just `actor::spawn`, in this
/// phase) are expected to publish the returned value themselves.
pub fn reduce(state: &WorkspaceState, action: Action) -> Result<WorkspaceState, ActionRejected> {
    match action {
        Action::RunPlanned { .. }
        | Action::RunStopRequested { .. }
        | Action::RunNodeStartRequested { .. }
        | Action::RunNodeStopRequested { .. }
        | Action::RunNodeDeleteRequested { .. } => run::reduce(state, action),

        Action::ContainerObserved { .. }
        | Action::ContainerActionSettled { .. }
        | Action::RunCreateSettled { .. }
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
