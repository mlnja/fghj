use std::sync::Arc;
use std::time::Duration;

use crate::daemon::control::DaemonControl;
use crate::util::time::now_ms;
use crate::{effects, resolver, supervisor};

/// Matches the UI's poll interval.
pub(crate) const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);

/// Coarser than `RECONCILE_INTERVAL`: full resolution of every `.fghj.yaml`,
/// for drift that only happens when someone edits a file.
pub(crate) const SYNC_RECONCILE_INTERVAL: Duration = Duration::from_secs(15);

/// Read-only toward Docker: reports container status into the actor so
/// out-of-band stops, removals and port changes show up. Never heals. Hosts,
/// resolver and raw-net routes belong to `effects::spawn_all`.
pub(crate) fn spawn_reconciler(daemon: Arc<DaemonControl>) {
    supervisor::supervise_forever("docker reconciler", reconcile_loop(daemon));
}

/// A `loop` that never breaks would type as `!`, which no `supervisor::Outcome`
/// covers.
async fn reconcile_loop(daemon: Arc<DaemonControl>) {
    let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
    loop {
        interval.tick().await;
        for (id, _) in daemon.registry.list() {
            if let Some(state) = daemon.registry.get(&id)
                && let Some(actor) = daemon.registry.actor(&id)
            {
                effects::docker::observe::report(&state.orchestrator, &actor).await;
            }
        }
        if daemon.is_active() {
            *daemon.last_reconcile_ms.lock().unwrap() = Some(now_ms());
        }
    }
}

/// Re-resolves each workspace and reports config drift into its actor. Runs
/// while idle too: it's graph metadata, not a side effect.
pub(crate) fn spawn_sync_reconciler(daemon: Arc<DaemonControl>) {
    supervisor::supervise_forever("config-sync reconciler", sync_reconcile_loop(daemon));
}

/// See `reconcile_loop`.
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
            let Some(actor) = daemon.registry.actor(&id) else {
                continue;
            };
            effects::docker::observe::report_config_drift(&state.orchestrator, &actor, &graph)
                .await;
        }
    }
}
