//! Volume *discovery*, not full desired/observed diffing: reports what
//! `docker::list_run_volumes` actually finds for a run into actor state
//! via `Action::VolumeObserved`, letting `reducer::observation`'s existing
//! bootstrap-on-first-observation logic (`VolumeInfo::desired` seeded from
//! the first `VolumeObserved` for a name never seen before) populate
//! `state::EnvState::volumes`.
//!
//! Deliberately not a full desired/observed diff with actuation: real
//! desired-state volume tracking would need
//! a volume's identity threaded through `EnvSpec`/`Action::EnvPlanned` from
//! the resolved `.fghj.yaml` graph, which `EnvSpec` doesn't carry (see
//! `state::env::EnvSpec`'s doc — it's deliberately thin)
//! and `runs::orchestrate` doesn't populate either (it builds every
//! `EnvState` with `volumes: BTreeMap::new()`). Plumbing real volume specs
//! through would mean
//! reaching into `RunRegistry::ensure_running`'s graph-resolution
//! internals. What this module gives instead:
//! every volume that actually exists in Docker for a run becomes visible
//! (`observed.exists == true`), with its `desired` bootstrapped to match —
//! so a volume that gets deleted out from under `fghj` by hand would *not*
//! currently show as "missing" (nothing re-reports `exists: false` for a
//! name that's simply absent from `list_run_volumes`'s result), only a
//! volume unexpectedly present becomes visible. That asymmetry is a known
//! limit, not an oversight.

use crate::action::Action;
use crate::actor::ActorHandle;
use crate::runs::RunRegistry;

/// Discovers the environment's volumes via Docker and reports each one
/// found.
pub async fn observe_volumes(runs: &RunRegistry, actor: &ActorHandle) {
    let names = runs.volume_names().await;
    dispatch_observed_volumes(actor, names).await;
}

/// The pure-ish dispatch half of `observe_volumes`, split out so it's
/// unit-testable without a real `RunRegistry`/Docker client — the actor is
/// still real (in-process, no Docker needed to spawn one).
async fn dispatch_observed_volumes(actor: &ActorHandle, names: Vec<String>) {
    for name in names {
        let _ = actor
            .dispatch(Action::VolumeObserved {
                volume_name: name,
                exists: true,
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::WorkspaceState;

    #[tokio::test]
    async fn dispatch_observed_volumes_reports_every_discovered_name() {
        let handle = crate::actor::spawn(crate::state::testing::workspace(vec![]));

        dispatch_observed_volumes(&handle, vec!["pgdata".into(), "cache".into()]).await;

        let current = handle.current();
        let volumes = &current.env.as_ref().unwrap().volumes;
        assert!(volumes["pgdata"].observed.exists);
        assert!(volumes["cache"].observed.exists);
    }

    #[tokio::test]
    async fn dispatch_observed_volumes_with_no_environment_is_a_silent_no_op() {
        let handle = crate::actor::spawn(WorkspaceState::default());

        dispatch_observed_volumes(&handle, vec!["pgdata".into()]).await;

        assert!(handle.current().env.is_none());
    }
}
