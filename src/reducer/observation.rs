//! Handles the five "report" `Action` variants dispatched by a polling or
//! convergence effect describing what's already true, rather than
//! requesting a change — see `action::Action`'s module doc for the
//! request/report split.
//!
//! Unlike `reducer::run`, nothing here ever returns `Err`: a report about a
//! run/node this workspace no longer tracks isn't a caller mistake to
//! reject, it's an unremarkable race between an effect's own polling
//! cadence and a node being deleted out from under it — by the time the
//! report arrives, there may be nothing left to update, and that's fine.
//! Treating it as a rejection would give a background poller something to
//! retry or log loudly about for no reason.

use crate::action::{Action, ActionRejected};
use crate::state::{VolumeDesired, VolumeInfo, VolumeObserved, WorkspaceState};

pub(super) fn reduce(
    state: &WorkspaceState,
    action: Action,
) -> Result<WorkspaceState, ActionRejected> {
    match action {
        Action::ContainerObserved {
            run_id,
            node_id,
            status,
            published_port,
            ip,
            ports,
        } => {
            let mut next = state.clone();
            if let Some(container) = next
                .runs
                .get_mut(&run_id)
                .and_then(|run| run.containers.get_mut(&node_id))
            {
                container.observed.status = status;
                container.observed.published_port = published_port;
                container.observed.ip = ip;
                container.observed.ports = ports;
            }
            Ok(next)
        }

        Action::ContainerActionSettled {
            run_id,
            node_id,
            result,
        } => {
            let mut next = state.clone();
            if let Some(run) = next.runs.get_mut(&run_id) {
                match result {
                    // A freshly re-observed container replaces whatever was
                    // there wholesale — this is the only place left (now
                    // that `effects::bridge` is gone) that ever learns the
                    // real post-action status/ports/routes, so a partial
                    // merge would leave stale fields no future report will
                    // ever correct.
                    Ok(Some(info)) => {
                        run.containers.insert(node_id, info);
                    }
                    // `None` only ever means a `Removing` action actually
                    // removed the container.
                    Ok(None) => {
                        run.containers.remove(&node_id);
                    }
                    Err(_) => {
                        if let Some(container) = run.containers.get_mut(&node_id) {
                            container.pending_action = None;
                        }
                    }
                }
            }
            Ok(next)
        }

        Action::RunCreateSettled { run_id, result } => {
            let mut next = state.clone();
            match result {
                Ok(run) => {
                    next.runs.insert(run_id, run);
                }
                Err(_) => {
                    if let Some(run) = next.runs.get_mut(&run_id) {
                        run.pending_create = None;
                    }
                }
            }
            Ok(next)
        }

        Action::VolumeObserved {
            run_id,
            volume_name,
            exists,
        } => {
            let mut next = state.clone();
            if let Some(run) = next.runs.get_mut(&run_id) {
                run.volumes
                    .entry(volume_name.clone())
                    .or_insert_with(|| VolumeInfo {
                        desired: VolumeDesired { name: volume_name },
                        observed: VolumeObserved::default(),
                    })
                    .observed
                    .exists = exists;
            }
            Ok(next)
        }

        Action::ConfigDriftObserved {
            run_id,
            node_id,
            drift,
        } => {
            let mut next = state.clone();
            if let Some(container) = next
                .runs
                .get_mut(&run_id)
                .and_then(|run| run.containers.get_mut(&node_id))
            {
                container.observed.sync = drift;
            }
            Ok(next)
        }

        other => unreachable!(
            "reducer::observation::reduce called with a non-observation action: {other:?}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        ContainerDesired, ContainerInfo, ContainerObserved, PendingAction, RunState, SyncStatus,
    };
    use std::collections::BTreeMap;

    fn container(node_id: &str) -> ContainerInfo {
        ContainerInfo {
            node_id: node_id.into(),
            desired: ContainerDesired {
                running: true,
                container_name: format!("fghj-{node_id}-1"),
                domain: format!("{node_id}.fghj.internal"),
                raw_domain: format!("{node_id}.fghj.raw.internal"),
                routes: vec![],
                additional_hosts: vec![],
                status_port: None,
                config_hash: "hash".into(),
            },
            observed: ContainerObserved::default(),
            pending_action: None,
        }
    }

    fn state_with_run(run_id: &str, containers: Vec<ContainerInfo>) -> WorkspaceState {
        let mut state = WorkspaceState::default();
        state.runs.insert(
            run_id.into(),
            RunState {
                run_id: run_id.into(),
                network: "fghj-net".into(),
                containers: containers
                    .into_iter()
                    .map(|c| (c.node_id.clone(), c))
                    .collect(),
                volumes: BTreeMap::new(),
                sidecar_container_name: "fghj-sidecar".into(),
                sidecar_ip: None,
                pending_create: None,
            },
        );
        state
    }

    #[test]
    fn container_observed_updates_status_and_port_and_ip_and_ports() {
        let state = state_with_run("default", vec![container("web")]);
        let next = reduce(
            &state,
            Action::ContainerObserved {
                run_id: "default".into(),
                node_id: "web".into(),
                status: "running".into(),
                published_port: Some(54321),
                ip: Some("172.20.0.5".into()),
                ports: BTreeMap::from([("http".into(), Some(54321)), ("db".into(), None)]),
            },
        )
        .unwrap();
        let c = &next.runs["default"].containers["web"];
        assert_eq!(c.observed.status, "running");
        assert_eq!(c.observed.published_port, Some(54321));
        assert_eq!(c.observed.ip.as_deref(), Some("172.20.0.5"));
        assert_eq!(c.observed.ports.get("http"), Some(&Some(54321)));
        assert_eq!(c.observed.ports.get("db"), Some(&None));
    }

    #[test]
    fn container_observed_for_unknown_node_is_a_silent_no_op() {
        let state = state_with_run("default", vec![]);
        let next = reduce(
            &state,
            Action::ContainerObserved {
                run_id: "default".into(),
                node_id: "ghost".into(),
                status: "running".into(),
                published_port: None,
                ip: None,
                ports: BTreeMap::new(),
            },
        )
        .unwrap();
        assert!(next.runs["default"].containers.is_empty());
    }

    #[test]
    fn container_observed_for_unknown_run_is_a_silent_no_op() {
        let state = WorkspaceState::default();
        let next = reduce(
            &state,
            Action::ContainerObserved {
                run_id: "missing".into(),
                node_id: "web".into(),
                status: "running".into(),
                published_port: None,
                ip: None,
                ports: BTreeMap::new(),
            },
        )
        .unwrap();
        assert!(next.runs.is_empty());
    }

    #[test]
    fn container_action_settled_replaces_the_container_with_the_settled_observation() {
        let mut web = container("web");
        web.pending_action = Some(PendingAction::Starting);
        let state = state_with_run("default", vec![web]);
        let mut fresh = container("web");
        fresh.observed.status = "running".into();
        fresh.observed.published_port = Some(54321);
        let next = reduce(
            &state,
            Action::ContainerActionSettled {
                run_id: "default".into(),
                node_id: "web".into(),
                result: Ok(Some(fresh)),
            },
        )
        .unwrap();
        let c = &next.runs["default"].containers["web"];
        assert!(c.pending_action.is_none());
        assert_eq!(c.observed.status, "running");
        assert_eq!(c.observed.published_port, Some(54321));
    }

    #[test]
    fn container_action_settled_clears_pending_action_on_failure() {
        let mut web = container("web");
        web.pending_action = Some(PendingAction::Stopping);
        let state = state_with_run("default", vec![web]);
        let next = reduce(
            &state,
            Action::ContainerActionSettled {
                run_id: "default".into(),
                node_id: "web".into(),
                result: Err("docker daemon unreachable".into()),
            },
        )
        .unwrap();
        assert!(
            next.runs["default"].containers["web"]
                .pending_action
                .is_none()
        );
    }

    #[test]
    fn container_action_settled_removes_the_container_when_a_removal_succeeds() {
        let mut web = container("web");
        web.pending_action = Some(PendingAction::Removing);
        let state = state_with_run("default", vec![web]);
        let next = reduce(
            &state,
            Action::ContainerActionSettled {
                run_id: "default".into(),
                node_id: "web".into(),
                result: Ok(None),
            },
        )
        .unwrap();
        assert!(!next.runs["default"].containers.contains_key("web"));
    }

    #[test]
    fn container_action_settled_keeps_the_container_when_a_removal_fails() {
        let mut web = container("web");
        web.pending_action = Some(PendingAction::Removing);
        let state = state_with_run("default", vec![web]);
        let next = reduce(
            &state,
            Action::ContainerActionSettled {
                run_id: "default".into(),
                node_id: "web".into(),
                result: Err("container in use".into()),
            },
        )
        .unwrap();
        let c = &next.runs["default"].containers["web"];
        assert!(c.pending_action.is_none());
    }

    #[test]
    fn run_create_settled_inserts_the_freshly_created_run_on_success() {
        let state = WorkspaceState::default();
        let created = RunState {
            run_id: "default".into(),
            network: "fghj-net-default".into(),
            containers: BTreeMap::from([("web".to_string(), container("web"))]),
            volumes: BTreeMap::new(),
            sidecar_container_name: "fghj-sidecar".into(),
            sidecar_ip: Some("172.20.0.2".into()),
            pending_create: None,
        };
        let next = reduce(
            &state,
            Action::RunCreateSettled {
                run_id: "default".into(),
                result: Ok(created),
            },
        )
        .unwrap();
        assert!(next.runs["default"].containers.contains_key("web"));
    }

    #[test]
    fn run_create_settled_clears_pending_create_without_touching_containers_on_failure() {
        let mut existing = state_with_run("default", vec![container("web")]);
        existing.runs.get_mut("default").unwrap().pending_create = Some(crate::state::RunSpec {
            run_id: None,
            flow: None,
        });
        let next = reduce(
            &existing,
            Action::RunCreateSettled {
                run_id: "default".into(),
                result: Err("docker daemon unreachable".into()),
            },
        )
        .unwrap();
        let run = &next.runs["default"];
        assert!(run.pending_create.is_none());
        assert!(run.containers.contains_key("web"));
    }

    #[test]
    fn volume_observed_creates_a_new_entry_when_none_existed() {
        let state = state_with_run("default", vec![]);
        let next = reduce(
            &state,
            Action::VolumeObserved {
                run_id: "default".into(),
                volume_name: "fghj-vol-web".into(),
                exists: true,
            },
        )
        .unwrap();
        let volume = &next.runs["default"].volumes["fghj-vol-web"];
        assert_eq!(volume.desired.name, "fghj-vol-web");
        assert!(volume.observed.exists);
    }

    #[test]
    fn volume_observed_updates_an_existing_entry_in_place() {
        let state = state_with_run("default", vec![]);
        let created = reduce(
            &state,
            Action::VolumeObserved {
                run_id: "default".into(),
                volume_name: "fghj-vol-web".into(),
                exists: true,
            },
        )
        .unwrap();
        let updated = reduce(
            &created,
            Action::VolumeObserved {
                run_id: "default".into(),
                volume_name: "fghj-vol-web".into(),
                exists: false,
            },
        )
        .unwrap();
        assert!(
            !updated.runs["default"].volumes["fghj-vol-web"]
                .observed
                .exists
        );
        assert_eq!(updated.runs["default"].volumes.len(), 1);
    }

    #[test]
    fn config_drift_observed_updates_the_containers_sync_status() {
        let state = state_with_run("default", vec![container("web")]);
        let next = reduce(
            &state,
            Action::ConfigDriftObserved {
                run_id: "default".into(),
                node_id: "web".into(),
                drift: SyncStatus::Drifted,
            },
        )
        .unwrap();
        assert_eq!(
            next.runs["default"].containers["web"].observed.sync,
            SyncStatus::Drifted
        );
    }
}
