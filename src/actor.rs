//! The per-workspace actor: applies `reducer::reduce` to every `Action`
//! serially and publishes the result. The only store of environment state.

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, watch};

use crate::action::{Action, ActionRejected};
use crate::reducer::reduce;
use crate::state::WorkspaceState;
use crate::supervisor;

type Reply = oneshot::Sender<Result<(), ActionRejected>>;

#[derive(Clone)]
pub struct ActorHandle {
    actions: mpsc::UnboundedSender<(Action, Reply)>,
    state: watch::Receiver<Arc<WorkspaceState>>,
}

impl ActorHandle {
    /// Returns once the reducer has run, not once effects have converged.
    pub async fn dispatch(&self, action: Action) -> Result<(), ActionRejected> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.actions.send((action, reply_tx)).is_err() {
            // The actor is gone: reads the same as the environment not existing.
            return Err(ActionRejected::NoEnvironment);
        }
        reply_rx.await.unwrap_or(Err(ActionRejected::NoEnvironment))
    }

    /// Fresh per caller: `changed()` is per-receiver, so shared receivers would
    /// steal each other's wakeups.
    pub fn subscribe(&self) -> watch::Receiver<Arc<WorkspaceState>> {
        self.state.clone()
    }

    pub fn current(&self) -> Arc<WorkspaceState> {
        self.state.borrow().clone()
    }
}

/// Runs until every handle is dropped. Unbounded: the reducer is pure and
/// never the slow part.
pub fn spawn(initial: WorkspaceState) -> ActorHandle {
    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<(Action, Reply)>();
    let (state_tx, state_rx) = watch::channel(Arc::new(initial));

    supervisor::supervise("workspace actor", async move {
        while let Some((action, reply)) = action_rx.recv().await {
            let current = state_tx.borrow().clone();
            let outcome = match reduce(&current, action) {
                Ok(next) => {
                    // Errors only when nobody is subscribed, which is fine.
                    let _ = state_tx.send(Arc::new(next));
                    Ok(())
                }
                Err(rejected) => Err(rejected),
            };
            let _ = reply.send(outcome);
        }
    });

    ActorHandle {
        actions: action_tx,
        state: state_rx,
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

    #[tokio::test]
    async fn dispatch_applies_the_reducer_and_returns_ok() {
        let handle = spawn(WorkspaceState::default());
        let result = handle.dispatch(Action::OwnerSet { owner: owner() }).await;
        assert!(result.is_ok());
        assert_eq!(handle.current().owner.as_ref().unwrap().uid, 501);
    }

    #[tokio::test]
    async fn dispatch_surfaces_a_reducer_rejection() {
        let handle = spawn(WorkspaceState::default());
        let result = handle
            .dispatch(Action::NodeStopRequested {
                node_id: "web".into(),
            })
            .await;
        assert_eq!(result.unwrap_err(), ActionRejected::NoEnvironment);
    }

    #[tokio::test]
    async fn subscribers_observe_state_published_after_they_subscribed() {
        let handle = spawn(WorkspaceState::default());
        let mut rx = handle.subscribe();
        assert!(rx.borrow().owner.is_none());

        handle
            .dispatch(Action::OwnerSet { owner: owner() })
            .await
            .unwrap();

        rx.changed().await.unwrap();
        assert_eq!(rx.borrow().owner.as_ref().unwrap().uid, 501);
    }

    #[tokio::test]
    async fn each_subscribe_call_returns_an_independent_receiver() {
        let handle = spawn(WorkspaceState::default());
        let mut first = handle.subscribe();
        handle
            .dispatch(Action::OwnerSet { owner: owner() })
            .await
            .unwrap();
        first.changed().await.unwrap();
        let second = handle.subscribe();
        assert_eq!(second.borrow().owner.as_ref().unwrap().uid, 501);
    }

    #[tokio::test]
    async fn dispatch_reports_not_found_once_the_actor_is_gone() {
        let (action_tx, action_rx) = mpsc::unbounded_channel::<(Action, Reply)>();
        drop(action_rx);
        let orphaned = ActorHandle {
            actions: action_tx,
            state: watch::channel(Arc::new(WorkspaceState::default())).1,
        };
        let result = orphaned.dispatch(Action::OwnerSet { owner: owner() }).await;
        assert_eq!(result.unwrap_err(), ActionRejected::NoEnvironment);
    }
}
