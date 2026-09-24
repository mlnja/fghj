//! The per-workspace actor: owns one `state::WorkspaceState` for its whole
//! life, applies the pure `reducer::reduce` to every dispatched `Action`
//! serially, and publishes the result for effects to observe. See the
//! architecture plan (rosy-soaring-teapot.md)'s "Per-workspace canonical
//! state + actor" section — this is the concrete implementation of that
//! design, and as of migration phase 5 the only store of run state in the
//! daemon: everything that used to keep its own copy (`runs::RunRegistry`,
//! `effects::bridge`) now reads this one and reports back to it.

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, watch};

use crate::action::{Action, ActionRejected};
use crate::reducer::reduce;
use crate::state::WorkspaceState;

type Reply = oneshot::Sender<Result<(), ActionRejected>>;

/// A cheap, `Clone`-able handle to a running workspace actor — the only
/// way anything outside this module talks to a workspace's state. Cloning
/// it never clones the state itself, just the two channel ends.
#[derive(Clone)]
pub struct ActorHandle {
    actions: mpsc::UnboundedSender<(Action, Reply)>,
    state: watch::Receiver<Arc<WorkspaceState>>,
}

impl ActorHandle {
    /// Dispatches `action` and waits for the reducer to apply it. Returns
    /// as soon as the (pure, in-memory) reducer has run — not once any
    /// effect has converged reality to match — see the plan's "HTTP
    /// handler contract" section for why that's the deliberate, fast
    /// contract every mutating endpoint is meant to rely on.
    pub async fn dispatch(&self, action: Action) -> Result<(), ActionRejected> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.actions.send((action, reply_tx)).is_err() {
            // The actor task is gone (workspace deregistered, or the
            // daemon is shutting down) — there's no reducer left to
            // accept or reject this, which reads the same to a caller as
            // "the thing you asked about doesn't exist any more."
            return Err(ActionRejected::RunNotFound);
        }
        reply_rx.await.unwrap_or(Err(ActionRejected::RunNotFound))
    }

    /// A fresh, independent `watch::Receiver`. Cloning this (rather than
    /// handing out the one this handle already holds) matters:
    /// `watch::Receiver::changed` is stateful per-receiver ("have I seen
    /// the latest value yet"), so two unrelated callers sharing one
    /// receiver would silently steal wakeups from each other.
    pub fn subscribe(&self) -> watch::Receiver<Arc<WorkspaceState>> {
        self.state.clone()
    }

    /// The latest published state, without waiting for a change.
    pub fn current(&self) -> Arc<WorkspaceState> {
        self.state.borrow().clone()
    }
}

/// Spawns the actor task that owns `initial` for the rest of its life,
/// returning a handle to talk to it. The task runs until every
/// `ActorHandle` (the one returned here, and every clone of it) is
/// dropped — at that point `actions.recv()` returns `None` and the loop,
/// and the task, exit.
///
/// The action channel is unbounded: the reducer is pure and in-memory, so
/// it's never the slow part of the system — see the plan's "Action
/// channel backpressure" open item. A sender that ever needed backpressure
/// here would itself indicate a misbehaving observer, better caught by
/// logging an unexpectedly large channel than by blocking on `send`.
pub fn spawn(initial: WorkspaceState) -> ActorHandle {
    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<(Action, Reply)>();
    let (state_tx, state_rx) = watch::channel(Arc::new(initial));

    tokio::spawn(async move {
        while let Some((action, reply)) = action_rx.recv().await {
            let current = state_tx.borrow().clone();
            let outcome = match reduce(&current, action) {
                Ok(next) => {
                    // `send` only errors when every receiver has been
                    // dropped, i.e. nobody currently cares about this
                    // workspace's state — not a reason to stop processing
                    // its actions.
                    let _ = state_tx.send(Arc::new(next));
                    Ok(())
                }
                Err(rejected) => Err(rejected),
            };
            // Ignored for the same reason: the caller may have stopped
            // waiting (dropped its `oneshot::Receiver`) without that being
            // this actor's problem.
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
            .dispatch(Action::RunNodeStartRequested {
                run_id: "missing".into(),
                node_id: "web".into(),
            })
            .await;
        assert_eq!(result.unwrap_err(), ActionRejected::RunNotFound);
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
        // Draining `first` doesn't affect a second, freshly-subscribed
        // receiver's ability to observe the already-published change.
        first.changed().await.unwrap();
        let second = handle.subscribe();
        assert_eq!(second.borrow().owner.as_ref().unwrap().uid, 501);
    }

    #[tokio::test]
    async fn dispatch_reports_not_found_once_the_actor_is_gone() {
        // Simulates the actor task having already exited (e.g. workspace
        // deregistered) by dropping its receiving end directly, rather
        // than spawning a real actor and racing its shutdown.
        let (action_tx, action_rx) = mpsc::unbounded_channel::<(Action, Reply)>();
        drop(action_rx);
        let orphaned = ActorHandle {
            actions: action_tx,
            state: watch::channel(Arc::new(WorkspaceState::default())).1,
        };
        let result = orphaned.dispatch(Action::OwnerSet { owner: owner() }).await;
        assert_eq!(result.unwrap_err(), ActionRejected::RunNotFound);
    }
}
