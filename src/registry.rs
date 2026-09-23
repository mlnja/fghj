//! Daemon-wide directory of currently-live per-workspace actors — see the
//! architecture plan (rosy-soaring-teapot.md)'s "The registry:
//! infrastructure, not decision state" section. Deliberately named
//! `ActorRegistry`, not `WorkspaceRegistry`: `daemon::WorkspaceRegistry`
//! already exists and still owns today's pre-migration
//! `RunRegistry`/`WorkspaceDb`/Docker-client wiring (unchanged by this
//! phase); this type only ever answers "which workspace actors currently
//! exist and how do I reach them," which is routing/discovery
//! infrastructure, never something a decision is made from. The two are
//! expected to merge once a later migration phase retires the old one.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::watch;

use crate::actor::ActorHandle;

/// Everything needed to reach one workspace's actor — currently just the
/// handle itself, kept as its own type (rather than using `ActorHandle`
/// directly as the registry's value) so a later phase can attach
/// routing-only metadata (e.g. the workspace's id or path) without
/// changing `ActorHandle` itself.
#[derive(Clone)]
pub struct WorkspaceHandle {
    pub actor: ActorHandle,
}

/// `id -> WorkspaceHandle`, plus its own `watch::Receiver` over that map so
/// a daemon-wide fanned-in effect (see `effects::run_fanned_in_effect`)
/// can notice a workspace being registered or deregistered and add/drop
/// its own per-workspace forwarder task for it.
pub struct ActorRegistry {
    workspaces: watch::Sender<Arc<BTreeMap<String, WorkspaceHandle>>>,
}

impl ActorRegistry {
    pub fn new() -> Self {
        let (workspaces, _unused_receiver) = watch::channel(Arc::new(BTreeMap::new()));
        Self { workspaces }
    }

    /// Registers (or replaces) the actor for `id`. Replacing is
    /// deliberately allowed, not rejected: a workspace can be
    /// re-registered after a reload without a separate "deregister first"
    /// dance.
    pub fn insert(&self, id: String, handle: WorkspaceHandle) {
        self.workspaces.send_modify(|workspaces| {
            Arc::make_mut(workspaces).insert(id, handle);
        });
    }

    pub fn remove(&self, id: &str) {
        self.workspaces.send_modify(|workspaces| {
            Arc::make_mut(workspaces).remove(id);
        });
    }

    pub fn get(&self, id: &str) -> Option<WorkspaceHandle> {
        self.workspaces.borrow().get(id).cloned()
    }

    /// A fresh receiver over the current *set* of registered workspaces —
    /// see `subscribe`'s callers in `effects` for why this needs to be a
    /// fresh receiver per caller, same reasoning as `ActorHandle::subscribe`.
    pub fn subscribe(&self) -> watch::Receiver<Arc<BTreeMap<String, WorkspaceHandle>>> {
        self.workspaces.subscribe()
    }
}

impl Default for ActorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor;
    use crate::state::WorkspaceState;

    fn handle() -> WorkspaceHandle {
        WorkspaceHandle {
            actor: actor::spawn(WorkspaceState::default()),
        }
    }

    #[test]
    fn get_returns_none_for_an_unregistered_workspace() {
        let registry = ActorRegistry::new();
        assert!(registry.get("missing").is_none());
    }

    #[tokio::test]
    async fn insert_then_get_returns_the_registered_handle() {
        let registry = ActorRegistry::new();
        registry.insert("aikido-core".into(), handle());
        assert!(registry.get("aikido-core").is_some());
    }

    #[tokio::test]
    async fn insert_replaces_an_existing_entry_of_the_same_id() {
        let registry = ActorRegistry::new();
        registry.insert("aikido-core".into(), handle());
        registry.insert("aikido-core".into(), handle());
        assert!(registry.get("aikido-core").is_some());
    }

    #[tokio::test]
    async fn remove_drops_the_entry() {
        let registry = ActorRegistry::new();
        registry.insert("aikido-core".into(), handle());
        registry.remove("aikido-core");
        assert!(registry.get("aikido-core").is_none());
    }

    #[test]
    fn remove_of_an_unregistered_workspace_is_a_no_op() {
        let registry = ActorRegistry::new();
        registry.remove("missing");
        assert!(registry.get("missing").is_none());
    }

    #[tokio::test]
    async fn subscribers_observe_a_subsequent_insert() {
        let registry = ActorRegistry::new();
        let mut rx = registry.subscribe();
        assert!(rx.borrow().is_empty());

        registry.insert("aikido-core".into(), handle());

        rx.changed().await.unwrap();
        assert!(rx.borrow().contains_key("aikido-core"));
    }

    #[tokio::test]
    async fn each_subscribe_call_returns_an_independent_receiver() {
        let registry = ActorRegistry::new();
        registry.insert("aikido-core".into(), handle());
        let first = registry.subscribe();
        let second = registry.subscribe();
        assert_eq!(first.borrow().len(), second.borrow().len());
    }
}
