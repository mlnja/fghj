//! The `Effect` traits and their drivers. `dns`, `hosts` and `raw_net` are
//! daemon-wide; the rest are wired per workspace in `wire_actor`.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::watch;

use crate::actor::ActorHandle;
use crate::state::WorkspaceState;
use crate::supervisor;

pub type ActorMap = Arc<BTreeMap<String, ActorHandle>>;

pub mod dns;
pub mod docker;
pub mod hosts;
pub mod persist;
pub mod raw_net;
pub mod routes;

/// Converges an external resource to a workspace's state. `extract` is separate
/// from `converge` so the driver can skip unchanged snapshots for every effect.
pub trait Effect: Send {
    type Snapshot: PartialEq + Clone + Send;

    fn extract(&self, state: &WorkspaceState) -> Self::Snapshot;

    fn converge(&mut self, snapshot: &Self::Snapshot) -> anyhow::Result<()>;
}

pub trait AsyncEffect: Send {
    type Snapshot: PartialEq + Clone + Send;

    fn extract(&self, state: &WorkspaceState) -> Self::Snapshot;

    fn converge(
        &mut self,
        snapshot: Self::Snapshot,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

pub async fn run_async_effect<E: AsyncEffect>(
    mut effect: E,
    mut rx: watch::Receiver<Arc<WorkspaceState>>,
    name: &str,
) {
    let mut last: Option<E::Snapshot> = None;
    loop {
        let snapshot = effect.extract(&rx.borrow());
        if last.as_ref() != Some(&snapshot) {
            match effect.converge(snapshot.clone()).await {
                Ok(()) => last = Some(snapshot),
                Err(e) => eprintln!("fghjd: effect {name} failed to converge: {e:#}"),
            }
        }
        if rx.changed().await.is_err() {
            break;
        }
    }
}

/// An effect rendering one shared OS resource from every workspace's state.
pub trait FannedInEffect: Send {
    type Snapshot: PartialEq + Clone + Send;

    fn extract(&self, states: &BTreeMap<String, Arc<WorkspaceState>>) -> Self::Snapshot;

    fn converge(&mut self, snapshot: &Self::Snapshot) -> anyhow::Result<()>;
}

/// Runs for the workspace actor's lifetime, skipping unchanged snapshots. A
/// failure is logged and retried on the next state change.
pub async fn run_effect<E: Effect>(
    mut effect: E,
    mut rx: watch::Receiver<Arc<WorkspaceState>>,
    name: &str,
) {
    let mut last: Option<E::Snapshot> = None;
    loop {
        let snapshot = effect.extract(&rx.borrow());
        if last.as_ref() != Some(&snapshot) {
            match effect.converge(&snapshot) {
                Ok(()) => last = Some(snapshot),
                Err(e) => eprintln!("fghjd: effect {name} failed to converge: {e:#}"),
            }
        }
        if rx.changed().await.is_err() {
            break;
        }
    }
}

/// One forwarder task per workspace pokes a shared `Notify`; each wake
/// re-reads every workspace, since `extract` needs them all anyway.
pub async fn run_fanned_in_effect<E: FannedInEffect>(
    mut effect: E,
    mut registry_rx: watch::Receiver<ActorMap>,
    name: &str,
) {
    let changed = Arc::new(tokio::sync::Notify::new());
    let mut forwarders: BTreeMap<String, tokio::task::AbortHandle> = BTreeMap::new();
    let mut last: Option<E::Snapshot> = None;

    loop {
        let workspaces = registry_rx.borrow().clone();

        forwarders.retain(|id, task| {
            let keep = workspaces.contains_key(id);
            if !keep {
                task.abort();
            }
            keep
        });
        for (id, handle) in workspaces.iter() {
            forwarders.entry(id.clone()).or_insert_with(|| {
                let mut rx = handle.subscribe();
                let changed = changed.clone();
                supervisor::supervise("actor change forwarder", async move {
                    while rx.changed().await.is_ok() {
                        changed.notify_one();
                    }
                })
            });
        }

        let states: BTreeMap<String, Arc<WorkspaceState>> = workspaces
            .iter()
            .map(|(id, handle)| (id.clone(), handle.current()))
            .collect();
        let snapshot = effect.extract(&states);
        if last.as_ref() != Some(&snapshot) {
            match effect.converge(&snapshot) {
                Ok(()) => last = Some(snapshot),
                Err(e) => eprintln!("fghjd: effect {name} failed to converge: {e:#}"),
            }
        }

        tokio::select! {
            _ = changed.notified() => {}
            result = registry_rx.changed() => {
                if result.is_err() {
                    break;
                }
            }
        }
    }

    for task in forwarders.into_values() {
        task.abort();
    }
}

pub struct EffectTasks {
    raw_net: tokio::task::AbortHandle,
    dns: tokio::task::AbortHandle,
    hosts: tokio::task::AbortHandle,
}

impl EffectTasks {
    pub fn abort_all(&self) {
        self.raw_net.abort();
        self.dns.abort();
        self.hosts.abort();
    }
}

pub fn spawn_all(dns_port: u16, registry_rx: watch::Receiver<ActorMap>) -> EffectTasks {
    EffectTasks {
        raw_net: supervisor::supervise_forever(
            "raw_net effect",
            run_fanned_in_effect(raw_net::RawNetEffect, registry_rx.clone(), "raw_net"),
        ),
        dns: supervisor::supervise_forever(
            "dns effect",
            run_fanned_in_effect(
                dns::DnsEffect { port: dns_port },
                registry_rx.clone(),
                "dns",
            ),
        ),
        hosts: supervisor::supervise_forever(
            "hosts effect",
            run_fanned_in_effect(hosts::HostsEffect, registry_rx, "hosts"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::actor;
    use crate::state::WorkspaceState;
    use std::time::Duration;

    struct RecordingEffect {
        converged: Arc<std::sync::Mutex<Vec<Option<u32>>>>,
    }

    impl Effect for RecordingEffect {
        type Snapshot = Option<u32>;

        fn extract(&self, state: &WorkspaceState) -> Self::Snapshot {
            state.owner.as_ref().map(|o| o.uid)
        }

        fn converge(&mut self, snapshot: &Self::Snapshot) -> anyhow::Result<()> {
            self.converged.lock().unwrap().push(*snapshot);
            Ok(())
        }
    }

    fn owner(uid: u32) -> crate::persistence::WorkspaceOwner {
        crate::persistence::WorkspaceOwner {
            uid,
            gid: 20,
            home: "/Users/dev".into(),
            ssh_auth_sock: None,
        }
    }

    async fn wait_until(mut check: impl FnMut() -> bool) {
        for _ in 0..200 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition never became true");
    }

    #[tokio::test]
    async fn run_effect_converges_once_for_the_initial_state() {
        let handle = actor::spawn(WorkspaceState::default());
        let converged = Arc::new(std::sync::Mutex::new(Vec::new()));
        let effect = RecordingEffect {
            converged: converged.clone(),
        };
        tokio::spawn(run_effect(effect, handle.subscribe(), "test"));

        wait_until(|| !converged.lock().unwrap().is_empty()).await;
        assert_eq!(converged.lock().unwrap().as_slice(), [None]);
    }

    #[tokio::test]
    async fn run_effect_converges_again_only_when_the_snapshot_actually_changes() {
        let handle = actor::spawn(WorkspaceState::default());
        let converged = Arc::new(std::sync::Mutex::new(Vec::new()));
        let effect = RecordingEffect {
            converged: converged.clone(),
        };
        tokio::spawn(run_effect(effect, handle.subscribe(), "test"));
        wait_until(|| !converged.lock().unwrap().is_empty()).await;

        handle
            .dispatch(Action::OwnerSet { owner: owner(501) })
            .await
            .unwrap();
        wait_until(|| converged.lock().unwrap().len() >= 2).await;
        handle
            .dispatch(Action::OwnerSet { owner: owner(501) })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        assert_eq!(converged.lock().unwrap().as_slice(), [None, Some(501)]);
    }

    struct FannedInSum {
        converged: Arc<std::sync::Mutex<Vec<u32>>>,
    }

    impl FannedInEffect for FannedInSum {
        type Snapshot = u32;

        fn extract(&self, states: &BTreeMap<String, Arc<WorkspaceState>>) -> Self::Snapshot {
            states
                .values()
                .filter_map(|s| s.owner.as_ref().map(|o| o.uid))
                .sum()
        }

        fn converge(&mut self, snapshot: &Self::Snapshot) -> anyhow::Result<()> {
            self.converged.lock().unwrap().push(*snapshot);
            Ok(())
        }
    }

    #[tokio::test]
    async fn fanned_in_effect_aggregates_across_every_registered_workspace() {
        let a = actor::spawn(WorkspaceState::default());
        let b = actor::spawn(WorkspaceState::default());
        let (registry, _rx) = watch::channel(Arc::new(BTreeMap::from([
            ("a".to_string(), a.clone()),
            ("b".to_string(), b.clone()),
        ])));

        let converged = Arc::new(std::sync::Mutex::new(Vec::new()));
        let effect = FannedInSum {
            converged: converged.clone(),
        };
        tokio::spawn(run_fanned_in_effect(effect, registry.subscribe(), "test"));

        a.dispatch(Action::OwnerSet { owner: owner(10) })
            .await
            .unwrap();
        b.dispatch(Action::OwnerSet { owner: owner(32) })
            .await
            .unwrap();

        wait_until(|| converged.lock().unwrap().last() == Some(&42)).await;
    }

    #[tokio::test]
    async fn fanned_in_effect_stops_watching_a_deregistered_workspace() {
        let a = actor::spawn(WorkspaceState::default());
        let (registry, _rx) =
            watch::channel(Arc::new(BTreeMap::from([("a".to_string(), a.clone())])));

        let converged = Arc::new(std::sync::Mutex::new(Vec::new()));
        let effect = FannedInSum {
            converged: converged.clone(),
        };
        tokio::spawn(run_fanned_in_effect(effect, registry.subscribe(), "test"));
        wait_until(|| !converged.lock().unwrap().is_empty()).await;

        registry.send_modify(|actors| {
            Arc::make_mut(actors).remove("a");
        });
        wait_until(|| converged.lock().unwrap().last() == Some(&0)).await;
        let snapshots_after_removal = converged.lock().unwrap().len();

        a.dispatch(Action::OwnerSet { owner: owner(99) })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(converged.lock().unwrap().len(), snapshots_after_removal);
    }
}
