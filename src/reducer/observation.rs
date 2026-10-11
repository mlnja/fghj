//! Handles the five "report" `Action` variants dispatched by a polling or
//! convergence effect describing what's already true, rather than
//! requesting a change — see `action::Action`'s module doc for the
//! request/report split.
//!
//! Unlike `reducer::env`, nothing here ever returns `Err`: a report about a
//! run/node this workspace no longer tracks isn't a caller mistake to
//! reject, it's an unremarkable race between an effect's own polling
//! cadence and a node being deleted out from under it — by the time the
//! report arrives, there may be nothing left to update, and that's fine.
//! Treating it as a rejection would give a background poller something to
//! retry or log loudly about for no reason.

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
            ip,
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
                container.observed.ip = ip;
                container.observed.ports = ports;
                container.observed.exit_code = exit_code;
            }
            Ok(next)
        }

        Action::ContainerActionSettled { node_id, result } => {
            let mut next = state.clone();
            if let Some(run) = next.env.as_mut() {
                match result {
                    // A freshly re-observed container replaces whatever was
                    // there wholesale — this is the only place that ever
                    // learns the real post-action status/ports/routes, so a partial
                    // merge would leave stale fields no future report will
                    // ever correct.
                    Ok(Some(mut info)) => {
                        // The mark comes off here rather than being left to
                        // whatever built `info` — settling *is* the action
                        // being over. `Orchestrator::stop_container` returns
                        // a clone of the container it was handed, which
                        // still carries the `Stopping` that triggered it, so
                        // inserting that verbatim left the node reading
                        // "stopping" forever even though Docker had already
                        // stopped it. Worse, every `*Requested` arm rejects
                        // a container with a pending action as
                        // `AlreadyInFlight`, so the node could then never be
                        // started, stopped or deleted again for the life of
                        // the daemon. `pending_action` is the reducer's own
                        // field; no effect gets a say in it.
                        info.pending_action = None;
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

        Action::EnvCreateSettled { result } => {
            let mut next = state.clone();
            // Every branch writes through the entry `EnvPlanned` created
            // rather than inserting: a report describes work on an
            // environment the reducer already knows about, and never brings
            // one into existence on its own.
            let present = next.env.is_some();
            match result {
                Ok(mut run) => {
                    if present {
                        clear_create_marks(&mut run);
                        next.env = Some(run);
                    }
                }
                // A top-up that failed partway still left containers
                // running; taking its partial state is the only way they
                // become visible to `GET /environment` and routable from the host.
                // `pending_create` is cleared explicitly rather than relying
                // on the partial carrying `None`, since it is a snapshot of
                // a working copy, not a freshly-built run.
                Err(EnvCreateError {
                    partial: Some(mut run),
                    ..
                }) => {
                    if present {
                        run.pending_create = None;
                        clear_create_marks(&mut run);
                        next.env = Some(*run);
                    }
                }
                Err(_) => {
                    if let Some(run) = next.env.as_mut() {
                        run.pending_create = None;
                        clear_create_marks(run);
                    }
                }
            }
            Ok(next)
        }

        Action::EnvCreateWorking { node_id, action } => {
            let mut next = state.clone();
            // Same write-through rule as `EnvCreateSettled` above.
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
            // Same write-through rule as `EnvCreateSettled` above — hence
            // `as_mut` rather than `get_or_insert`.
            if let Some(run) = next.env.as_mut() {
                run.network = network;
                run.sidecar_container_name = sidecar_container_name;
                run.sidecar_ip = sidecar_ip;
                // The node's work is done, so its `EnvCreateWorking` mark
                // comes off with it.
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

/// A `starting_placeholder` that nothing has replaced: no container was
/// ever created for it.
fn is_placeholder(container: &ContainerInfo) -> bool {
    container.desired.container_name.is_empty()
}

/// Takes off every `EnvCreateWorking` mark once the create is over — the
/// settled state is the truth now, and a mark left behind would keep its
/// node reading "starting…" (and rejecting actions) forever. A placeholder
/// for a node that never got a container goes with it.
fn clear_create_marks(run: &mut EnvState) {
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
    fn container_observed_updates_status_and_port_and_ip_and_ports() {
        let state = workspace(vec![container("web")]);
        let next = reduce(
            &state,
            Action::ContainerObserved {
                node_id: "web".into(),
                status: "running".into(),
                published_port: Some(54321),
                ip: Some("172.20.0.5".into()),
                ports: BTreeMap::from([("http".into(), Some(54321)), ("db".into(), None)]),
                exit_code: None,
            },
        )
        .unwrap();
        let c = &next.env.as_ref().unwrap().containers["web"];
        assert_eq!(c.observed.status, "running");
        assert_eq!(c.observed.published_port, Some(54321));
        assert_eq!(c.observed.ip.as_deref(), Some("172.20.0.5"));
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
                ip: None,
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
                ip: None,
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

    /// `Orchestrator::stop_container` builds its return value by cloning the
    /// `ContainerInfo` it was handed, which is the one carrying the
    /// `Stopping` that triggered the call in the first place. Settling must
    /// clear it anyway: leaving it set stuck the node at "stopping" with the
    /// container already exited, and every later start/stop/delete request
    /// for it was then rejected as `AlreadyInFlight`.
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

    /// The success path replaces the placeholder `EnvPlanned` left behind
    /// with the real, fully-built run. It writes *through* that entry
    /// rather than inserting unconditionally — see
    /// `create_reports_never_bring_an_environment_into_existence`.
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

    /// B7: a top-up that fails on node 3 leaves nodes 1-2 running. They are
    /// already in Docker and in SQLite; without adopting the partial they
    /// would be absent from reducer state, which is what `GET /environment` and
    /// `state::query::resolve_route` both read.
    #[test]
    fn run_create_settled_adopts_the_partial_state_of_a_failed_top_up() {
        let mut existing = workspace(vec![container("web")]);
        existing.env.as_mut().unwrap().pending_create =
            Some(crate::state::EnvSpec::Flow("app/main".into()));

        // `web` was already up; `api` came up during this top-up before
        // `worker` failed.
        let partial = EnvState {
            network: "fghj-net-default".into(),
            containers: BTreeMap::from([
                ("web".to_string(), container("web")),
                ("api".to_string(), container("api")),
            ]),
            volumes: BTreeMap::new(),
            sidecar_container_name: "fghj-sidecar".into(),
            sidecar_ip: Some("172.20.0.2".into()),
            // A working copy, not a freshly-built run — the reducer must
            // clear this itself rather than assume it arrives clear.
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

    /// Per-node crash safety: a create reports each container the moment it
    /// comes up, so a daemon that dies halfway still has every
    /// already-running container written down. Without it those containers
    /// would be running unrecorded — fghj manufacturing the very
    /// `Orphaned` state its observer exists to surface.
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

    /// `EnvCreateProgress` and `EnvCreateSettled` only ever update the
    /// environment `EnvPlanned` created; a report with nothing to update
    /// is dropped rather than inventing one.
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

    /// A switch can take minutes and refuses every node action meanwhile;
    /// the node it is on right now has to say so, including one that has
    /// no container yet.
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

    /// Giving up on a node takes its mark off, and a placeholder that never
    /// became a container goes with it rather than lingering as a ghost.
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

    /// However the create ends, nothing it marked may stay marked: every
    /// node action on a marked node is refused as already in flight.
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
