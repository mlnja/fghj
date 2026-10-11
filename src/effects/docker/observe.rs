//! Reports containers that changed for reasons fghj didn't cause, as
//! `Action::ContainerObserved`. Runs on the reconciler's tick rather than its
//! own timer. Drift is only reported: `converge` reads `desired`, never
//! `observed`.

use crate::action::Action;
use crate::actor::ActorHandle;
use crate::orchestrator::Orchestrator;
use crate::orchestrator::observe::DriftReport;
use crate::resolver::Graph;
use crate::state::ContainerObserved;

use super::volumes;

pub async fn report(orchestrator: &Orchestrator, actor: &ActorHandle) {
    let Some(env) = actor.current().env.clone() else {
        return;
    };
    for (node_id, observed) in orchestrator.inspect_containers(&env).await {
        let _ = actor
            .dispatch(container_observed(&node_id, &observed))
            .await;
    }
    volumes::observe_volumes(orchestrator, actor).await;
}

/// Config drift, on the slower `spawn_sync_reconciler` schedule.
pub async fn report_config_drift(orchestrator: &Orchestrator, actor: &ActorHandle, graph: &Graph) {
    let Some(env) = actor.current().env.clone() else {
        return;
    };
    for report in orchestrator.config_drift(graph, &env).await {
        let _ = actor.dispatch(config_drift_observed(&report)).await;
    }
}

fn config_drift_observed(report: &DriftReport) -> Action {
    Action::ConfigDriftObserved {
        node_id: report.node_id.clone(),
        drift: report.sync,
    }
}

/// Takes `ContainerObserved` only, so an observation can't carry `desired`.
fn container_observed(node_id: &str, observed: &ContainerObserved) -> Action {
    Action::ContainerObserved {
        node_id: node_id.to_string(),
        status: observed.status.clone(),
        published_port: observed.published_port,
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
        let action = container_observed(&c.node_id, &c.observed);
        match action {
            Action::ContainerObserved {
                node_id,
                status,
                published_port,
                ports,
                exit_code: _,
            } => {
                assert_eq!(node_id, "web");
                assert_eq!(status, "running");
                assert_eq!(published_port, Some(8080));
                assert_eq!(ports.get("http"), Some(&Some(8080)));
            }
            other => panic!("expected ContainerObserved, got {other:?}"),
        }
    }

    #[test]
    fn config_drift_observed_carries_every_verdict_through() {
        use crate::state::SyncStatus;
        let verdict = |sync| match config_drift_observed(&DriftReport {
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
        let action = container_observed(&observed.node_id, &observed.observed);
        handle.dispatch(action).await.unwrap();

        let current = handle.current();
        let web = &current.env.as_ref().unwrap().containers["web"];
        assert_eq!(web.observed.status, "exited");
        assert!(web.desired.running);
    }
}
