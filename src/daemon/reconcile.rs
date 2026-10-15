use std::sync::Arc;
use std::time::Duration;

use crate::action::Action;
use crate::daemon::control::DaemonControl;
use crate::daemon::heal::Healer;
use crate::state::NodeCondition;
use crate::util::time::now_ms;
use crate::{daemon_log, effects, resolver, supervisor};

/// Matches the UI's poll interval.
pub(crate) const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);

/// Coarser than `RECONCILE_INTERVAL`: full resolution of every `.fghj.yaml`,
/// for drift that only happens when someone edits a file.
pub(crate) const SYNC_RECONCILE_INTERVAL: Duration = Duration::from_secs(15);

/// Reports container status into the actor so out-of-band stops, removals and
/// port changes show up, then restarts what crashed (`heal`) while active.
/// Hosts, resolver and raw-net routes belong to `effects::spawn_all`.
pub(crate) fn spawn_reconciler(daemon: Arc<DaemonControl>) {
    supervisor::supervise_forever("docker reconciler", reconcile_loop(daemon));
}

/// A `loop` that never breaks would type as `!`, which no `supervisor::Outcome`
/// covers.
async fn reconcile_loop(daemon: Arc<DaemonControl>) {
    let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
    let mut healer = Healer::default();
    loop {
        interval.tick().await;
        for (id, _) in daemon.registry.list() {
            let (Some(state), Some(actor)) = (daemon.registry.get(&id), daemon.registry.actor(&id))
            else {
                continue;
            };
            effects::docker::observe::report(&state.orchestrator, &actor).await;
            if !daemon.is_active() {
                continue;
            }
            let Some(env) = actor.current().env.clone() else {
                continue;
            };
            if let Some((node_id, condition, attempt)) =
                healer.pick(&id, &env, std::time::Instant::now())
            {
                daemon_log::info(if condition == NodeCondition::Interrupted {
                    format!(
                        "fghjd: {node_id} went down while Docker was out of sight, starting it again (attempt {attempt})"
                    )
                } else {
                    format!("fghjd: {node_id} crashed, restarting it (attempt {attempt})")
                });
                let _ = actor.dispatch(Action::NodeStartRequested { node_id }).await;
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
