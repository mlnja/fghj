//! The background loops that keep recorded state honest against Docker.

use std::sync::Arc;
use std::time::Duration;

use crate::daemon::control::DaemonControl;
use crate::util::time::now_ms;
use crate::{effects, resolver, supervisor};

/// How often the background reconciler re-inspects live containers. Kept in
/// step with the frontend's `/runs` poll interval (see `App.svelte`) so the
/// UI is essentially never stale.
pub(crate) const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);

/// How often `spawn_sync_reconciler` re-resolves each workspace's
/// `.fghj.yaml` and recomputes config-drift hashes. Deliberately much
/// coarser than `RECONCILE_INTERVAL`: unlike `refresh` (a handful of Docker
/// inspect calls), this re-runs full CUE resolution — parsing every
/// `.fghj.yaml` in the workspace from scratch — which is real work not worth
/// repeating every second just to catch drift that, by definition, only
/// happens when someone edits a config file by hand.
pub(crate) const SYNC_RECONCILE_INTERVAL: Duration = Duration::from_secs(15);

/// Background loop, analogous to a Kubernetes controller's reconcile loop
/// but read-only with respect to Docker: on each tick it re-inspects every
/// workspace's live containers and updates their recorded status, published
/// port, and routes (see `RunRegistry::inspect_containers`) so drift caused by someone
/// `docker stop`/`rm`-ing a container by hand, or Docker itself moving a
/// container to a different ephemeral host port on a restart it initiated
/// (restart policy, `dockerd` restarting), shows up — and routes correctly —
/// on its own, without a `fghjd` restart. It never recreates or restarts a
/// container itself — no self-healing there.
///
/// `/etc/hosts`, macOS's `/etc/resolver` and raw-zone NAT routes are not
/// this loop's job: the effects `effects::spawn_all` starts own them
/// (`effects::hosts::HostsEffect`, `effects::dns::DnsEffect`,
/// `effects::raw_net::RawNetEffect`). Two schedules writing the same
/// `pf`/`/etc/hosts`/`/etc/resolver` state is the bug class documented in
/// `raw_net::macos`'s module doc, so nothing here touches them.
pub(crate) fn spawn_reconciler(daemon: Arc<DaemonControl>) {
    supervisor::supervise_forever("docker reconciler", reconcile_loop(daemon));
}

/// Split out of `spawn_reconciler` so the body has a declared `()` return
/// type: the `loop` below never breaks, so as a bare async block it
/// would have type `!` and satisfy no `supervisor::Outcome` impl.
async fn reconcile_loop(daemon: Arc<DaemonControl>) {
    let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
    loop {
        interval.tick().await;
        for (id, _) in daemon.registry.list() {
            if let Some(state) = daemon.registry.get(&id) {
                // Re-inspects every recorded container and reports what
                // Docker actually says into the actor — see
                // `effects::docker::observe`'s module doc for why this
                // is the loop's only Docker read.
                if let Some(handle) = daemon.registry.actors().get(&id) {
                    effects::docker::observe::report(&state.runs, &handle.actor).await;
                }
            }
        }
        if daemon.is_active() {
            *daemon.last_reconcile_ms.lock().unwrap() = Some(now_ms());
        }
    }
}

/// The config-drift counterpart to `spawn_reconciler`: on its own, much
/// slower interval (`SYNC_RECONCILE_INTERVAL`), re-resolves each wired
/// workspace's `.fghj.yaml` from disk, compares that freshly-resolved graph
/// against the config each live container was actually last started with
/// (`RunRegistry::config_drift`), and reports the verdicts into the
/// workspace actor as `Action::ConfigDriftObserved`. Runs regardless of
/// `daemon.is_active()` — sync status is informational graph metadata, not
/// a routing/`/etc/hosts` side effect, so there's no "idle fghjd" reason to
/// skip it the way `spawn_reconciler` skips its `/etc/hosts`/`/etc/resolver`
/// sync.
pub(crate) fn spawn_sync_reconciler(daemon: Arc<DaemonControl>) {
    supervisor::supervise_forever("config-sync reconciler", sync_reconcile_loop(daemon));
}

/// Split out of `spawn_sync_reconciler` so the body has a declared `()` return
/// type: the `loop` below never breaks, so as a bare async block it
/// would have type `!` and satisfy no `supervisor::Outcome` impl.
async fn sync_reconcile_loop(daemon: Arc<DaemonControl>) {
    let mut interval = tokio::time::interval(SYNC_RECONCILE_INTERVAL);
    loop {
        interval.tick().await;
        for (id, path) in daemon.registry.list() {
            let Some(state) = daemon.registry.get(&id) else {
                continue;
            };
            let Ok(graph) = resolver::resolve_universe_async(path).await else {
                continue;
            };
            let Some(handle) = daemon.registry.actors().get(&id) else {
                continue;
            };
            effects::docker::observe::report_config_drift(&state.runs, &handle.actor, &graph).await;
        }
    }
}
