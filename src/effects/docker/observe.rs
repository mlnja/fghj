//! Closes the gap `effects::docker::converge`'s module doc names: since
//! `effects::bridge`/`Action::WorkspaceMirrored` were deleted (migration
//! phase 5), nothing dispatches `Action::ContainerObserved` for a container
//! that changed state for a reason neither `converge` nor an HTTP-dispatched
//! request caused (crashed, `docker stop`'d by hand, a restart-policy-
//! triggered restart) — so `observed != desired` never became visible in
//! the new system's state, only in the old one.
//!
//! Deliberately does **not** run its own independent Docker-inspection
//! timer. `daemon::spawn_reconciler` already inspects every container's
//! real status once a second via `runs::RunRegistry::refresh` — kept alive
//! after migration phase 5 because it's still load-bearing for live HTTPS
//! routing (`WorkspaceRegistry::resolve_route` reads the old
//! `runs::RunRegistry` state directly, deliberately left on the old system).
//! A second, independent Docker-polling loop here would double the real
//! Docker API load for the exact same information `refresh` already just
//! fetched, with no benefit — the "never leave two mechanisms driving the
//! same resource concurrently" rule from the migration plan is about
//! *actuation* (two things deciding to start/stop the same container), not
//! about a single read being reported to two readers, so reusing `refresh`'s
//! already-fresh result here doesn't violate it.
//!
//! `report` is the "-> ContainerObserved" half of the plan's "Docker
//! poller -> ContainerObserved" module: `daemon::spawn_reconciler` calls
//! `runs.refresh().await` (the poller, unchanged) and then this function
//! (the translation), once per tick, once per workspace.
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
use crate::runs::{self, RunRegistry};

use super::volumes;

/// Reports every container and volume `runs` (just refreshed by the
/// caller) currently knows about into `actor`'s new-system state.
pub async fn report(runs: &RunRegistry, actor: &ActorHandle) {
    for run in runs.list() {
        for c in &run.containers {
            let _ = actor.dispatch(container_observed(&run.run_id, c)).await;
        }
        volumes::observe_run_volumes(runs, actor, &run.run_id).await;
    }
}

/// Pure translation from the old system's per-container snapshot to the
/// report `Action` the new system understands — split out from `report` so
/// it's unit-testable without a real `RunRegistry`/Docker client.
fn container_observed(run_id: &str, c: &runs::ContainerInfo) -> Action {
    Action::ContainerObserved {
        run_id: run_id.to_string(),
        node_id: c.node_id.clone(),
        status: c.status.clone(),
        published_port: c.published_port,
        // The old `runs::ContainerInfo` never tracked a container's
        // network-internal IP; nothing here has one to report either.
        ip: None,
        ports: c.ports.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::{ContainerInfo, PortRoute};
    use std::collections::BTreeMap;

    fn container(node_id: &str, status: &str) -> ContainerInfo {
        ContainerInfo {
            node_id: node_id.into(),
            container_name: format!("fghj-{node_id}-1"),
            status: status.into(),
            published_port: Some(8080),
            domain: format!("{node_id}.fghj.internal"),
            raw_domain: format!("{node_id}.fghj.raw.internal"),
            routes: Vec::<PortRoute>::new(),
            additional_hosts: vec![],
            ports: BTreeMap::from([("http".to_string(), Some(8080))]),
            status_port: Some("http".into()),
            config_hash: "hash".into(),
            synced: None,
            pending_action: None,
        }
    }

    #[test]
    fn container_observed_translates_status_and_ports_without_touching_desired() {
        let c = container("web", "running");
        let action = container_observed("default", &c);
        match action {
            Action::ContainerObserved {
                run_id,
                node_id,
                status,
                published_port,
                ip,
                ports,
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

    #[tokio::test]
    async fn report_dispatches_observed_for_a_crashed_container_without_changing_desired() {
        use crate::state::{
            ContainerDesired, ContainerInfo as NewContainerInfo, ContainerObserved, RunState,
            WorkspaceState,
        };

        let mut state = WorkspaceState::default();
        state.runs.insert(
            "default".into(),
            RunState {
                run_id: "default".into(),
                network: "fghj-net".into(),
                containers: BTreeMap::from([(
                    "web".to_string(),
                    NewContainerInfo {
                        node_id: "web".into(),
                        desired: ContainerDesired {
                            running: true,
                            container_name: "fghj-web-1".into(),
                            domain: "web.fghj.internal".into(),
                            raw_domain: "web.fghj.raw.internal".into(),
                            routes: vec![],
                            additional_hosts: vec![],
                            status_port: Some("http".into()),
                            config_hash: "hash".into(),
                        },
                        observed: ContainerObserved::default(),
                        pending_action: None,
                    },
                )]),
                volumes: BTreeMap::new(),
                sidecar_container_name: "fghj-sidecar".into(),
                sidecar_ip: None,
                pending_create: None,
            },
        );
        let handle = crate::actor::spawn(state);

        let action = container_observed("default", &container("web", "exited"));
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
