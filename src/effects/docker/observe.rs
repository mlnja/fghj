//! Reports containers that changed state for a reason neither `converge`
//! nor an HTTP-dispatched request caused (crashed, `docker stop`'d by hand,
//! a restart-policy-triggered restart) as `Action::ContainerObserved`, so
//! `observed != desired` becomes visible in actor state.
//!
//! Deliberately does **not** run its own independent Docker-inspection
//! timer. `daemon::spawn_reconciler` calls `report` once a second, and
//! `report` does the inspecting itself (`RunRegistry::inspect_containers`);
//! a second, independent Docker-polling loop here would double the real
//! Docker API load for the exact same information.
//!
//! This is the *only* way a container's real status reaches anything
//! that acts on it: live HTTPS routing and DNS answering read the reducer
//! state these reports feed (`state::query`), not `runs::RunRegistry`. If
//! this stops being called, routes go stale.
//!
//! Container drift is deliberately only ever *reported*, never fed back
//! into `converge`: `effects::docker::converge::DockerConvergeEffect::extract`
//! derives its convergence snapshot purely from `ContainerInfo::desired`
//! (and `pending_action`/`pending_create`), never `observed` — so a
//! container `report` finds crashed becomes visible to the UI
//! (`observed.status != desired.running`-style drift) without `converge`
//! ever trying to restart it. See `effects::docker::policy::DockerHealPolicy`
//! for the switch that would change that, deliberately unused for now.

use crate::action::Action;
use crate::actor::ActorHandle;
use crate::resolver::Graph;
use crate::runs::RunRegistry;
use crate::runs::observe::DriftReport;
use crate::state::ContainerObserved;

use super::volumes;

/// Re-inspects every container the actor currently records and reports
/// what Docker actually says about it — plus each run's real volumes —
/// back into the actor's state: the inspection takes the actor's state as
/// its input and its results go straight out as actions — one read, one
/// writer.
pub async fn report(runs: &RunRegistry, actor: &ActorHandle) {
    let state = actor.current();
    for (run_id, observed) in runs.inspect_containers(&state.runs).await {
        for (node_id, observed) in observed {
            let _ = actor
                .dispatch(container_observed(&run_id, &node_id, &observed))
                .await;
        }
        volumes::observe_run_volumes(runs, actor, &run_id).await;
    }
}

/// The config-drift counterpart to [`report`], driven on its own much
/// slower schedule by `daemon::spawn_sync_reconciler` (re-resolving every
/// `.fghj.yaml` is real work; re-inspecting a container is not). `graph` is
/// the freshly-resolved workspace the caller just read off disk.
///
/// This is what makes `Action::ConfigDriftObserved` reach the reducer at
/// all: before it existed, `RunRegistry::refresh_sync_status` wrote drift
/// into the *old* state only, so `ContainerObserved::sync` stayed `Unknown`
/// forever and the UI's "desired ≠ actual" badge never lit up.
pub async fn report_config_drift(runs: &RunRegistry, actor: &ActorHandle, graph: &Graph) {
    let state = actor.current();
    for report in runs.config_drift(graph, &state.runs).await {
        let _ = actor.dispatch(config_drift_observed(&report)).await;
    }
}

/// Pure translation from a drift verdict to the report `Action` the new
/// system understands — split out from `report_config_drift` for the same
/// reason `container_observed` is split out from `report`.
fn config_drift_observed(report: &DriftReport) -> Action {
    Action::ConfigDriftObserved {
        run_id: report.run_id.clone(),
        node_id: report.node_id.clone(),
        drift: report.sync,
    }
}

/// Pure projection of a freshly re-inspected container into the report
/// `Action` the reducer understands — split out from `report` so it's
/// unit-testable without a real `RunRegistry`/Docker client. Takes a
/// `ContainerObserved`, not a whole `ContainerInfo`, so it is structurally
/// incapable of carrying anything from `desired`: this is an observation,
/// and the reducer must never learn what fghj wants from something
/// claiming to say what Docker did.
fn container_observed(run_id: &str, node_id: &str, observed: &ContainerObserved) -> Action {
    Action::ContainerObserved {
        run_id: run_id.to_string(),
        node_id: node_id.to_string(),
        status: observed.status.clone(),
        published_port: observed.published_port,
        // `RunRegistry::inspect_containers` doesn't inspect a container's
        // network-internal address, so there's none to report here.
        ip: None,
        ports: observed.ports.clone(),
        exit_code: observed.exit_code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ContainerInfo;
    use std::collections::BTreeMap;

    fn container(node_id: &str, status: &str) -> ContainerInfo {
        let mut c = crate::state::testing::container(node_id);
        c.desired.status_port = Some("http".into());
        c.observed.status = status.into();
        c.observed.published_port = Some(8080);
        c.observed.ports = BTreeMap::from([("http".to_string(), Some(8080))]);
        c
    }

    #[test]
    fn container_observed_translates_status_and_ports_without_touching_desired() {
        let c = container("web", "running");
        let action = container_observed("default", &c.node_id, &c.observed);
        match action {
            Action::ContainerObserved {
                run_id,
                node_id,
                status,
                published_port,
                ip,
                ports,
                exit_code: _,
            } => {
                assert_eq!(run_id, "default");
                assert_eq!(node_id, "web");
                assert_eq!(status, "running");
                assert_eq!(published_port, Some(8080));
                assert_eq!(ip, None);
                assert_eq!(ports.get("http"), Some(&Some(8080)));
            }
            other => panic!("expected ContainerObserved, got {other:?}"),
        }
    }

    /// Every verdict reaches the reducer unchanged — `Orphaned` included.
    /// It used to arrive as `Unknown`, which is what made a vanished node
    /// invisible downstream no matter what the drift check had decided.
    #[test]
    fn config_drift_observed_carries_every_verdict_through() {
        use crate::state::SyncStatus;
        let verdict = |sync| match config_drift_observed(&DriftReport {
            run_id: "default".into(),
            node_id: "web".into(),
            sync,
        }) {
            Action::ConfigDriftObserved { drift, .. } => drift,
            other => panic!("expected ConfigDriftObserved, got {other:?}"),
        };
        for sync in [
            SyncStatus::Synced,
            SyncStatus::Drifted,
            SyncStatus::Unknown,
            SyncStatus::Orphaned,
        ] {
            assert_eq!(verdict(sync), sync);
        }
    }

    #[tokio::test]
    async fn report_dispatches_observed_for_a_crashed_container_without_changing_desired() {
        let mut web = crate::state::testing::container("web");
        web.desired.status_port = Some("http".into());
        let state = crate::state::testing::workspace(vec![web]);
        let handle = crate::actor::spawn(state);

        let observed = container("web", "exited");
        let action = container_observed("default", &observed.node_id, &observed.observed);
        handle.dispatch(action).await.unwrap();

        let current = handle.current();
        let web = &current.runs["default"].containers["web"];
        // Drift is now visible in `observed`...
        assert_eq!(web.observed.status, "exited");
        // ...but `desired.running` — what `converge`'s `extract()` actually
        // reads — is completely untouched, matching the observer-only
        // policy.
        assert!(web.desired.running);
    }
}
