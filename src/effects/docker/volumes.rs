//! Volume *discovery*, not full desired/observed diffing: reports what
//! `docker::list_run_volumes` actually finds for a run into the new system
//! via `Action::VolumeObserved`, letting `reducer::observation`'s existing
//! bootstrap-on-first-observation logic (`VolumeInfo::desired` seeded from
//! the first `VolumeObserved` for a name never seen before) populate
//! `state::RunState::volumes`.
//!
//! Deliberately scoped down from the plan's literal "volume desired/
//! observed diff+actuation": real desired-state volume tracking would need
//! a volume's identity threaded through `RunSpec`/`Action::RunPlanned` from
//! the resolved `.fghj.yaml` graph, which `RunSpec` doesn't carry (see
//! `state::run::RunSpec`'s doc — it's deliberately thin, `{run_id, flow}`)
//! and `effects/docker/converge.rs::mirror_run` doesn't populate either
//! (`volumes: BTreeMap::new()` — "the old system never tracked volumes as
//! state of their own"). Plumbing real volume specs through would mean
//! reaching into `RunRegistry::start`'s graph-resolution internals, which
//! is out of scope for this phase and risky to touch given how sidecar-
//! routing-sensitive that code path is. What this module gives instead:
//! every volume that actually exists in Docker for a run becomes visible
//! (`observed.exists == true`), with its `desired` bootstrapped to match —
//! so a volume that gets deleted out from under `fghj` by hand would *not*
//! currently show as "missing" (nothing re-reports `exists: false` for a
//! name that's simply absent from `list_run_volumes`'s result), only a
//! volume unexpectedly present becomes visible. That asymmetry is a known,
//! documented limit of this phase, not an oversight.

use crate::action::Action;
use crate::actor::ActorHandle;
use crate::runs::RunRegistry;

/// Discovers `run_id`'s volumes via Docker and reports each one found.
pub async fn observe_run_volumes(runs: &RunRegistry, actor: &ActorHandle, run_id: &str) {
    let names = runs.volume_names(run_id).await;
    dispatch_observed_volumes(actor, run_id, names).await;
}

/// The pure-ish dispatch half of `observe_run_volumes`, split out so it's
/// unit-testable without a real `RunRegistry`/Docker client — the actor is
/// still real (in-process, no Docker needed to spawn one).
async fn dispatch_observed_volumes(actor: &ActorHandle, run_id: &str, names: Vec<String>) {
    for name in names {
        let _ = actor
            .dispatch(Action::VolumeObserved {
                run_id: run_id.to_string(),
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
        let mut state = WorkspaceState::default();
        state.runs.insert(
            "default".into(),
            crate::state::RunState {
                run_id: "default".into(),
                network: "fghj-net".into(),
                containers: Default::default(),
                volumes: Default::default(),
                sidecar_container_name: "fghj-sidecar".into(),
                sidecar_ip: None,
                pending_create: None,
            },
        );
        let handle = crate::actor::spawn(state);

        dispatch_observed_volumes(&handle, "default", vec!["pgdata".into(), "cache".into()]).await;

        let current = handle.current();
        let volumes = &current.runs["default"].volumes;
        assert!(volumes["pgdata"].observed.exists);
        assert!(volumes["cache"].observed.exists);
    }

    #[tokio::test]
    async fn dispatch_observed_volumes_for_unknown_run_is_a_silent_no_op() {
        let handle = crate::actor::spawn(WorkspaceState::default());

        dispatch_observed_volumes(&handle, "missing", vec!["pgdata".into()]).await;

        assert!(handle.current().runs.is_empty());
    }
}
