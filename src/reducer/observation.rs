//! Report actions. Never rejected: a report about something already gone is
//! a normal race with polling, not an error.

use crate::action::{Action, ActionRejected};
use crate::state::{
    ContainerInfo, EnvCreateError, EnvState, VolumeDesired, VolumeInfo, VolumeObserved,
    WorkspaceState,
};

pub(super) fn reduce(
    state: &WorkspaceState,
    action: Action,
) -> Result<WorkspaceState, ActionRejected> {
    match action {
        Action::ContainerObserved {
            node_id,
            status,
            published_port,
            ports,
            exit_code,
        } => {
            let mut next = state.clone();
            if let Some(container) = next
                .env
                .as_mut()
                .and_then(|run| run.containers.get_mut(&node_id))
            {
                container.observed.status = status;
                container.observed.published_port = published_port;
                container.observed.ports = ports;
                container.observed.exit_code = exit_code;
            }
            Ok(next)
        }

        Action::ContainerActionSettled { node_id, result } => {
            let mut next = state.clone();
            if let Some(run) = next.env.as_mut() {
                match result {
                    // Replaced wholesale: this is the only place real post-action state arrives.
                    Ok(Some(mut info)) => {
                        // Cleared here: `stop_container` returns a clone still marked `Stopping`, and
                        // a stuck mark makes every later request `AlreadyInFlight`.
                        info.pending_action = None;
                        run.containers.insert(node_id, info);
                    }
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

        Action::EnvCreateSettled { result } => {
            let mut next = state.clone();
            // Reports update the environment `EnvPlanned` created; they never create one.
            let present = next.env.is_some();
            match result {
                Ok(mut run) => {
                    if present {
                        clear_create_marks(&mut run);
                        next.env = Some(run);
                    }
                }
                // A partially failed top-up left containers running; adopt them.
                Err(EnvCreateError {
                    partial: Some(mut run),
                    ..
                }) => {
                    if present {
                        clear_create_marks(&mut run);
                        next.env = Some(*run);
                    }
                }
                Err(_) => {
                    if let Some(run) = next.env.as_mut() {
                        clear_create_marks(run);
                    }
                }
            }
            Ok(next)
        }

        Action::EnvCreateWorking { node_id, action } => {
            let mut next = state.clone();
            if let Some(run) = next.env.as_mut() {
                match (action, run.containers.get_mut(&node_id)) {
                    (Some(action), Some(container)) => container.pending_action = Some(action),
                    (Some(_), None) => {
                        run.containers
                            .insert(node_id.clone(), super::env::starting_placeholder(&node_id));
                    }
                    (None, Some(container)) if is_placeholder(container) => {
                        run.containers.remove(&node_id);
                    }
                    (None, Some(container)) => container.pending_action = None,
                    (None, None) => {}
                }
            }
            Ok(next)
        }

        Action::EnvCreateProgress {
            network,
            sidecar_container_name,
            sidecar_ip,
            mut info,
        } => {
            let mut next = state.clone();
            if let Some(run) = next.env.as_mut() {
                run.network = network;
                run.sidecar_container_name = sidecar_container_name;
                run.sidecar_ip = sidecar_ip;
                info.pending_action = None;
                run.containers.insert(info.node_id.clone(), info);
            }
            Ok(next)
        }

        Action::VolumeObserved {
            volume_name,
            exists,
        } => {
            let mut next = state.clone();
            if let Some(run) = next.env.as_mut() {
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

        Action::ConfigDriftObserved { node_id, drift } => {
            let mut next = state.clone();
            if let Some(container) = next
                .env
                .as_mut()
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

fn is_placeholder(container: &ContainerInfo) -> bool {
    container.desired.container_name.is_empty()
}

/// Clears every create mark once the create is over, and drops placeholders
/// that never got a container. The settled state is a copy taken mid-create,
/// so it still carries `pending_create`.
fn clear_create_marks(run: &mut EnvState) {
    run.pending_create = None;
    run.containers.retain(|_, c| !is_placeholder(c));
    for container in run.containers.values_mut() {
        container.pending_action = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{EnvState, PendingAction, SyncStatus};
    use std::collections::BTreeMap;

    use crate::state::testing::{container, workspace};

    #[test]
    fn container_observed_updates_status_and_port_and_ports() {
        let state = workspace(vec![container("web")]);
        let next = reduce(
            &state,
            Action::ContainerObserved {
                node_id: "web".into(),
                status: "running".into(),
                published_port: Some(54321),
                ports: BTreeMap::from([("http".into(), Some(54321)), ("db".into(), None)]),
                exit_code: None,
            },
        )
        .unwrap();
        let c = &next.env.as_ref().unwrap().containers["web"];
        assert_eq!(c.observed.status, "running");
        assert_eq!(c.observed.published_port, Some(54321));
        assert_eq!(c.observed.ports.get("http"), Some(&Some(54321)));
        assert_eq!(c.observed.ports.get("db"), Some(&None));
    }

    #[test]
    fn container_observed_for_unknown_node_is_a_silent_no_op() {
        let state = workspace(vec![]);
        let next = reduce(
            &state,
            Action::ContainerObserved {
                node_id: "ghost".into(),
                status: "running".into(),
                published_port: None,
                ports: BTreeMap::new(),
                exit_code: None,
            },
        )
        .unwrap();
        assert!(next.env.as_ref().unwrap().containers.is_empty());
    }

    #[test]
    fn container_observed_with_no_environment_is_a_silent_no_op() {
        let state = WorkspaceState::default();
        let next = reduce(
            &state,
            Action::ContainerObserved {
                node_id: "web".into(),
                status: "running".into(),
                published_port: None,
                ports: BTreeMap::new(),
                exit_code: None,
            },
        )
        .unwrap();
        assert!(next.env.is_none());
    }

    #[test]
    fn container_action_settled_replaces_the_container_with_the_settled_observation() {
        let mut web = container("web");
        web.pending_action = Some(PendingAction::Starting);
        let state = workspace(vec![web]);
        let mut fresh = container("web");
        fresh.observed.status = "running".into();
        fresh.observed.published_port = Some(54321);
        let next = reduce(
            &state,
            Action::ContainerActionSettled {
                node_id: "web".into(),
                result: Ok(Some(fresh)),
            },
        )
        .unwrap();
        let c = &next.env.as_ref().unwrap().containers["web"];
        assert!(c.pending_action.is_none());
        assert_eq!(c.observed.status, "running");
        assert_eq!(c.observed.published_port, Some(54321));
    }

    #[test]
    fn container_action_settled_clears_a_pending_action_the_effect_echoed_back() {
        let mut web = container("web");
        web.pending_action = Some(PendingAction::Stopping);
        let state = workspace(vec![web.clone()]);
        let mut settled = web;
        settled.desired.running = false;
        settled.observed.status = "exited".into();
        let next = reduce(
            &state,
            Action::ContainerActionSettled {
                node_id: "web".into(),
                result: Ok(Some(settled)),
            },
        )
        .unwrap();
        let c = &next.env.as_ref().unwrap().containers["web"];
        assert!(c.pending_action.is_none());
        assert_eq!(c.observed.status, "exited");
        assert!(!c.desired.running);
    }

    #[test]
    fn container_action_settled_clears_pending_action_on_failure() {
        let mut web = container("web");
        web.pending_action = Some(PendingAction::Stopping);
        let state = workspace(vec![web]);
        let next = reduce(
            &state,
            Action::ContainerActionSettled {
                node_id: "web".into(),
                result: Err("docker daemon unreachable".into()),
            },
        )
        .unwrap();
        assert!(
            next.env.as_ref().unwrap().containers["web"]
                .pending_action
                .is_none()
        );
    }

    #[test]
    fn container_action_settled_removes_the_container_when_a_removal_succeeds() {
        let mut web = container("web");
        web.pending_action = Some(PendingAction::Removing);
        let state = workspace(vec![web]);
        let next = reduce(
            &state,
            Action::ContainerActionSettled {
                node_id: "web".into(),
                result: Ok(None),
            },
        )
        .unwrap();
        assert!(!next.env.as_ref().unwrap().containers.contains_key("web"));
    }

    #[test]
    fn container_action_settled_keeps_the_container_when_a_removal_fails() {
        let mut web = container("web");
        web.pending_action = Some(PendingAction::Removing);
        let state = workspace(vec![web]);
        let next = reduce(
            &state,
            Action::ContainerActionSettled {
                node_id: "web".into(),
                result: Err("container in use".into()),
            },
        )
        .unwrap();
        let c = &next.env.as_ref().unwrap().containers["web"];
        assert!(c.pending_action.is_none());
    }

    #[test]
    fn run_create_settled_replaces_the_planned_entry_on_success() {
        let state = workspace(vec![]);
        let created = EnvState {
            network: "fghj-net-default".into(),
            containers: BTreeMap::from([("web".to_string(), container("web"))]),
            volumes: BTreeMap::new(),
            sidecar_container_name: "fghj-sidecar".into(),
            sidecar_ip: Some("172.20.0.2".into()),
            pending_create: None,
        };
        let next = reduce(
            &state,
            Action::EnvCreateSettled {
                result: Ok(created),
            },
        )
        .unwrap();
        assert!(next.env.as_ref().unwrap().containers.contains_key("web"));
    }

    #[test]
    fn a_successful_create_clears_the_pending_create_its_result_was_copied_with() {
        let mut state = workspace(vec![]);
        let plan = crate::state::EnvSpec::Flow("app/main".into());
        state.env.as_mut().unwrap().pending_create = Some(plan.clone());
        let mut created = state.env.clone().unwrap();
        created.containers.insert("web".into(), container("web"));
        let next = reduce(
            &state,
            Action::EnvCreateSettled {
                result: Ok(created),
            },
        )
        .unwrap();
        let env = next.env.unwrap();
        assert!(env.pending_create.is_none());
        assert!(env.containers.contains_key("web"));
    }

    #[test]
    fn run_create_settled_clears_pending_create_without_touching_containers_on_failure() {
        let mut existing = workspace(vec![container("web")]);
        existing.env.as_mut().unwrap().pending_create =
            Some(crate::state::EnvSpec::Flow("app/main".into()));
        let next = reduce(
            &existing,
            Action::EnvCreateSettled {
                result: Err(EnvCreateError::bare("docker daemon unreachable")),
            },
        )
        .unwrap();
        let run = &next.env.as_ref().unwrap();
        assert!(run.pending_create.is_none());
        assert!(run.containers.contains_key("web"));
    }

    #[test]
    fn run_create_settled_adopts_the_partial_state_of_a_failed_top_up() {
        let mut existing = workspace(vec![container("web")]);
        existing.env.as_mut().unwrap().pending_create =
            Some(crate::state::EnvSpec::Flow("app/main".into()));

        let partial = EnvState {
            network: "fghj-net-default".into(),
            containers: BTreeMap::from([
                ("web".to_string(), container("web")),
                ("api".to_string(), container("api")),
            ]),
            volumes: BTreeMap::new(),
            sidecar_container_name: "fghj-sidecar".into(),
            sidecar_ip: Some("172.20.0.2".into()),
            pending_create: Some(crate::state::EnvSpec::Flow("app/main".into())),
        };

        let next = reduce(
            &existing,
            Action::EnvCreateSettled {
                result: Err(EnvCreateError {
                    message: "worker: image pull failed".into(),
                    partial: Some(Box::new(partial)),
                }),
            },
        )
        .unwrap();

        let run = &next.env.as_ref().unwrap();
        assert!(run.pending_create.is_none());
        assert!(run.containers.contains_key("web"));
        assert!(
            run.containers.contains_key("api"),
            "the container that did come up must be visible"
        );
    }

    #[test]
    fn volume_observed_creates_a_new_entry_when_none_existed() {
        let state = workspace(vec![]);
        let next = reduce(
            &state,
            Action::VolumeObserved {
                volume_name: "fghj-vol-web".into(),
                exists: true,
            },
        )
        .unwrap();
        let volume = &next.env.as_ref().unwrap().volumes["fghj-vol-web"];
        assert_eq!(volume.desired.name, "fghj-vol-web");
        assert!(volume.observed.exists);
    }

    #[test]
    fn volume_observed_updates_an_existing_entry_in_place() {
        let state = workspace(vec![]);
        let created = reduce(
            &state,
            Action::VolumeObserved {
                volume_name: "fghj-vol-web".into(),
                exists: true,
            },
        )
        .unwrap();
        let updated = reduce(
            &created,
            Action::VolumeObserved {
                volume_name: "fghj-vol-web".into(),
                exists: false,
            },
        )
        .unwrap();
        assert!(
            !updated.env.as_ref().unwrap().volumes["fghj-vol-web"]
                .observed
                .exists
        );
        assert_eq!(updated.env.as_ref().unwrap().volumes.len(), 1);
    }

    #[test]
    fn config_drift_observed_updates_the_containers_sync_status() {
        let state = workspace(vec![container("web")]);
        let next = reduce(
            &state,
            Action::ConfigDriftObserved {
                node_id: "web".into(),
                drift: SyncStatus::Drifted,
            },
        )
        .unwrap();
        assert_eq!(
            next.env.as_ref().unwrap().containers["web"].observed.sync,
            SyncStatus::Drifted
        );
    }

    #[test]
    fn run_create_progress_records_a_node_as_soon_as_it_comes_up() {
        let state = workspace(vec![]);
        let next = reduce(
            &state,
            Action::EnvCreateProgress {
                network: "fghj-default".into(),
                sidecar_container_name: "fghj-default-sidecar".into(),
                sidecar_ip: Some("172.30.0.2".into()),
                info: container("web"),
            },
        )
        .unwrap();
        let run = &next.env.as_ref().unwrap();
        assert_eq!(run.network, "fghj-default");
        assert_eq!(run.sidecar_ip.as_deref(), Some("172.30.0.2"));
        assert!(run.containers.contains_key("web"));
    }

    #[test]
    fn create_reports_never_bring_an_environment_into_existence() {
        let state = WorkspaceState::default();

        let after_progress = reduce(
            &state,
            Action::EnvCreateProgress {
                network: "fghj-default".into(),
                sidecar_container_name: "fghj-default-sidecar".into(),
                sidecar_ip: None,
                info: container("web"),
            },
        )
        .unwrap();
        assert!(after_progress.env.is_none());

        let after_ok = reduce(
            &state,
            Action::EnvCreateSettled {
                result: Ok(EnvState {
                    ..Default::default()
                }),
            },
        )
        .unwrap();
        assert!(after_ok.env.is_none());

        let after_partial = reduce(
            &state,
            Action::EnvCreateSettled {
                result: Err(EnvCreateError {
                    message: "node 2 of 3 failed".into(),
                    partial: Some(Box::new(EnvState {
                        ..Default::default()
                    })),
                }),
            },
        )
        .unwrap();
        assert!(after_partial.env.is_none());
    }

    fn working(node_id: &str, action: Option<PendingAction>) -> Action {
        Action::EnvCreateWorking {
            node_id: node_id.into(),
            action,
        }
    }

    #[test]
    fn run_create_working_marks_the_node_being_worked_on() {
        let state = workspace(vec![container("web")]);
        let next = reduce(&state, working("web", Some(PendingAction::Stopping))).unwrap();
        let next = reduce(&next, working("db", Some(PendingAction::Starting))).unwrap();
        let run = &next.env.as_ref().unwrap();
        assert_eq!(
            run.containers["web"].pending_action,
            Some(PendingAction::Stopping)
        );
        assert_eq!(
            run.containers["db"].pending_action,
            Some(PendingAction::Starting)
        );
    }

    #[test]
    fn run_create_working_none_unmarks_and_drops_a_placeholder() {
        let state = workspace(vec![container("web")]);
        let next = reduce(&state, working("web", Some(PendingAction::Starting))).unwrap();
        let next = reduce(&next, working("db", Some(PendingAction::Starting))).unwrap();
        let next = reduce(&next, working("web", None)).unwrap();
        let next = reduce(&next, working("db", None)).unwrap();
        let run = &next.env.as_ref().unwrap();
        assert_eq!(run.containers["web"].pending_action, None);
        assert!(!run.containers.contains_key("db"));
    }

    #[test]
    fn run_create_progress_takes_the_working_mark_off() {
        let state = workspace(vec![]);
        let next = reduce(&state, working("web", Some(PendingAction::Starting))).unwrap();
        let mut info = container("web");
        info.pending_action = Some(PendingAction::Starting);
        let next = reduce(
            &next,
            Action::EnvCreateProgress {
                network: "fghj-net".into(),
                sidecar_container_name: "fghj-sidecar".into(),
                sidecar_ip: None,
                info,
            },
        )
        .unwrap();
        assert_eq!(
            next.env.as_ref().unwrap().containers["web"].pending_action,
            None
        );
    }

    #[test]
    fn run_create_settled_err_clears_every_working_mark() {
        let mut state = workspace(vec![container("web")]);
        state.env.as_mut().unwrap().pending_create =
            Some(crate::state::EnvSpec::Flow("app/main".into()));
        let next = reduce(&state, working("web", Some(PendingAction::Stopping))).unwrap();
        let next = reduce(&next, working("db", Some(PendingAction::Starting))).unwrap();
        let next = reduce(
            &next,
            Action::EnvCreateSettled {
                result: Err(EnvCreateError::bare("boom")),
            },
        )
        .unwrap();
        let run = &next.env.as_ref().unwrap();
        assert_eq!(run.pending_create, None);
        assert_eq!(run.containers["web"].pending_action, None);
        assert!(!run.containers.contains_key("db"));
    }
}
