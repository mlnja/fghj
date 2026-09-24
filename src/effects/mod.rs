//! The `Effect` trait and its two generic drivers — see the architecture
//! plan (rosy-soaring-teapot.md)'s "Effects — two shapes" section. `dns`,
//! `hosts`, and `raw_net` are the daemon-wide fanned-in effects migrated so
//! far (`spawn_all` below spawns all three together). `docker::converge` is
//! the first per-workspace `Effect` (migration phase 4) — wired directly
//! into `daemon::WorkspaceRegistry::wire_actor` rather than `spawn_all`,
//! since it's driven off one workspace's own state, not every workspace's
//! combined.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::watch;

use crate::registry::WorkspaceHandle;
use crate::state::WorkspaceState;

pub mod dns;
pub mod docker;
pub mod hosts;
pub mod persist;
pub mod raw_net;
pub mod routes;

/// Converges some external resource (a file, an OS firewall table, Docker)
/// to match a workspace's desired state — the "make reality match state"
/// half of the Redux model this daemon is moving to. `extract`/`converge`
/// are kept separate, rather than one `fn run(&mut self, state)`, so the
/// *driver* (below) can own the "did anything actually change" check
/// uniformly for every effect, instead of each one reimplementing its own
/// diff the way `dns.rs`/`hosts_file.rs`/`raw_net::macos` each currently
/// do independently.
pub trait Effect: Send {
    type Snapshot: PartialEq + Clone + Send;

    fn extract(&self, state: &WorkspaceState) -> Self::Snapshot;

    fn converge(&mut self, snapshot: &Self::Snapshot) -> anyhow::Result<()>;
}

/// `Effect` for a resource whose convergence is itself asynchronous — the
/// database, principally, whose writes go through `spawn_blocking`.
///
/// A separate trait rather than making `Effect::converge` async: the three
/// OS-resource effects (`dns`, `hosts`, `raw_net`) converge by writing a
/// file or reloading a table, with nothing to await, and making them async
/// would buy them nothing while costing every one of them a desugaring.
/// The two drivers are otherwise identical, including the "skip converge
/// when the snapshot is unchanged" rule that is the whole point of
/// splitting `extract` from `converge`.
pub trait AsyncEffect: Send {
    type Snapshot: PartialEq + Clone + Send;

    fn extract(&self, state: &WorkspaceState) -> Self::Snapshot;

    fn converge(
        &mut self,
        snapshot: Self::Snapshot,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

/// `run_effect` for an `AsyncEffect`. Same contract, including leaving
/// `last` untouched on failure so the next state change retries.
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

/// The daemon-wide counterpart of `Effect`, for an effect that renders one
/// shared OS resource from *every* currently-registered workspace's state
/// combined (DNS resolver files, the hosts file, pf/raw-net — see the
/// plan's "(B) Daemon-wide fanned-in effects" section) rather than a
/// single workspace's.
pub trait FannedInEffect: Send {
    type Snapshot: PartialEq + Clone + Send;

    fn extract(&self, states: &BTreeMap<String, Arc<WorkspaceState>>) -> Self::Snapshot;

    fn converge(&mut self, snapshot: &Self::Snapshot) -> anyhow::Result<()>;
}

/// Drives a single-workspace `Effect` for as long as `rx` has a live
/// sender — i.e. for that workspace actor's whole lifetime. Skips
/// `converge` (and whatever I/O it does) whenever the newly observed
/// snapshot is unchanged from the last one actually converged, so an
/// unrelated action on the same workspace never causes redundant work.
///
/// A `converge` failure is logged and *not* retried immediately: `last` is
/// left at its old value, so the next actual state change re-attempts it —
/// and since the actor publishes a new state on every successfully-applied
/// action regardless of which fields it touched, "the next actual state
/// change" in practice means "the next action dispatched to this
/// workspace," not a fixed timer. This matches today's `raw_net`/`dns`
/// behavior of "log and continue," per the plan's open item on effect
/// crash/restart semantics, resolved here rather than left open.
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
            // The workspace actor is gone — nothing left to converge.
            break;
        }
    }
}

/// Drives a `FannedInEffect` across every workspace currently registered
/// in `registry_rx`. Rather than juggle a dynamically-sized set of
/// `watch::Receiver`s directly in a `select!`, every per-workspace change
/// is funneled through one shared "something changed" signal
/// (`tokio::sync::Notify`), fed by one small forwarder task per
/// currently-registered workspace — itself spawned/aborted as the
/// registry's own workspace set changes. On every wake, the effect
/// re-reads *all* currently-registered workspaces' latest state fresh;
/// there's no attempt to track *which* workspace changed, only *that* one
/// did, since `extract` needs the full set every time regardless.
pub async fn run_fanned_in_effect<E: FannedInEffect>(
    mut effect: E,
    mut registry_rx: watch::Receiver<Arc<BTreeMap<String, WorkspaceHandle>>>,
    name: &str,
) {
    let changed = Arc::new(tokio::sync::Notify::new());
    let mut forwarders: BTreeMap<String, tokio::task::JoinHandle<()>> = BTreeMap::new();
    let mut last: Option<E::Snapshot> = None;

    loop {
        let workspaces = registry_rx.borrow().clone();

        // Reconcile the forwarder-task set against the current workspace
        // set: spawn one for each newly-registered workspace, abort one
        // for each deregistered workspace. Aborting (rather than letting
        // it run to completion) is correct here — the workspace it was
        // watching no longer exists, so it has nothing useful left to
        // report.
        forwarders.retain(|id, task| {
            let keep = workspaces.contains_key(id);
            if !keep {
                task.abort();
            }
            keep
        });
        for (id, handle) in workspaces.iter() {
            forwarders.entry(id.clone()).or_insert_with(|| {
                let mut rx = handle.actor.subscribe();
                let changed = changed.clone();
                tokio::spawn(async move {
                    while rx.changed().await.is_ok() {
                        changed.notify_one();
                    }
                })
            });
        }

        let states: BTreeMap<String, Arc<WorkspaceState>> = workspaces
            .iter()
            .map(|(id, handle)| (id.clone(), handle.actor.current()))
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

/// Every daemon-wide fanned-in effect currently migrated off `daemon.rs`'s
/// old `spawn_reconciler` — see `spawn_all`. Bundled into one struct (rather
/// than three loose `JoinHandle`s in `daemon::ActiveResources`) so
/// `abort_all` can guarantee all three ever stop together, the same
/// atomically-together shutdown `DaemonControl::deactivate` already relied
/// on for the single `raw_net_task` before this phase.
pub struct EffectTasks {
    raw_net: tokio::task::JoinHandle<()>,
    dns: tokio::task::JoinHandle<()>,
    hosts: tokio::task::JoinHandle<()>,
}

impl EffectTasks {
    pub fn abort_all(&self) {
        self.raw_net.abort();
        self.dns.abort();
        self.hosts.abort();
    }
}

/// Spawns the raw-net, DNS-routing, and `/etc/hosts` fanned-in effects
/// together, all driven off the same daemon-wide `registry_rx` — the single
/// call site `DaemonControl::activate` uses in place of the three separate
/// `hosts_file::sync`/`dns::install_os_resolver_config` calls and the one
/// `raw_net` task spawn it used to do individually. `dns_port` is the port
/// `dns::bind` already bound the DNS server to by the time this is called.
pub fn spawn_all(
    dns_port: u16,
    registry_rx: watch::Receiver<Arc<BTreeMap<String, WorkspaceHandle>>>,
) -> EffectTasks {
    EffectTasks {
        raw_net: tokio::spawn(run_fanned_in_effect(
            raw_net::RawNetEffect,
            registry_rx.clone(),
            "raw_net",
        )),
        dns: tokio::spawn(run_fanned_in_effect(
            dns::DnsEffect { port: dns_port },
            registry_rx.clone(),
            "dns",
        )),
        hosts: tokio::spawn(run_fanned_in_effect(
            hosts::HostsEffect,
            registry_rx,
            "hosts",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::actor;
    use crate::registry::{ActorRegistry, WorkspaceHandle};
    use crate::state::WorkspaceState;
    use std::time::Duration;

    /// Converges whenever the owner's uid changes, recording every
    /// distinct snapshot it ever converged.
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

        // OwnerSet with the same uid a caller last observed still produces
        // a *new* WorkspaceState value (a fresh clone), but the effect's
        // own extracted Snapshot is unchanged, so no second convergence
        // should happen.
        handle
            .dispatch(Action::OwnerSet { owner: owner(501) })
            .await
            .unwrap();
        wait_until(|| converged.lock().unwrap().len() >= 2).await;
        handle
            .dispatch(Action::OwnerSet { owner: owner(501) })
            .await
            .unwrap();
        // Give a redundant convergence a chance to (wrongly) show up.
        tokio::time::sleep(Duration::from_millis(30)).await;

        assert_eq!(converged.lock().unwrap().as_slice(), [None, Some(501)]);
    }

    /// Sums the uids of every workspace's owner, recording every distinct
    /// snapshot it's ever asked to converge (in order).
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
        let registry = ActorRegistry::new();
        let a = actor::spawn(WorkspaceState::default());
        let b = actor::spawn(WorkspaceState::default());
        registry.insert("a".into(), WorkspaceHandle { actor: a.clone() });
        registry.insert("b".into(), WorkspaceHandle { actor: b.clone() });

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

        // However many intermediate snapshots the two racing dispatches
        // above produce, the last one converged must reflect both
        // workspaces once both updates have landed.
        wait_until(|| converged.lock().unwrap().last() == Some(&42)).await;
    }

    #[tokio::test]
    async fn fanned_in_effect_stops_watching_a_deregistered_workspace() {
        let registry = ActorRegistry::new();
        let a = actor::spawn(WorkspaceState::default());
        registry.insert("a".into(), WorkspaceHandle { actor: a.clone() });

        let converged = Arc::new(std::sync::Mutex::new(Vec::new()));
        let effect = FannedInSum {
            converged: converged.clone(),
        };
        tokio::spawn(run_fanned_in_effect(effect, registry.subscribe(), "test"));
        wait_until(|| !converged.lock().unwrap().is_empty()).await;

        registry.remove("a");
        // The removal is itself a registry-set change, which the driver
        // reacts to by rebuilding its forwarder set (aborting "a"'s) and
        // reconverging once against the now-empty workspace set.
        wait_until(|| converged.lock().unwrap().last() == Some(&0)).await;
        let snapshots_after_removal = converged.lock().unwrap().len();

        // A dispatch to the now-deregistered workspace must not trigger
        // another convergence — its forwarder was aborted above.
        a.dispatch(Action::OwnerSet { owner: owner(99) })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(converged.lock().unwrap().len(), snapshots_after_removal);
    }
}
