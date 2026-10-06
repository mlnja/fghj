//! Handles the five "request" `Action` variants that originate from an
//! HTTP handler expressing intent (plan/stop a run, start/stop/delete one
//! node) — see `action::Action`'s module doc for the request/report split.
//! This is where the in-flight dedup that used to live in
//! `RunRegistry::pending`/`PendingGuard` (`src/runs.rs:711-736`) now lives:
//! a plain `container.pending_action.is_some()` field read, since a pure
//! reducer never needs a `Mutex<HashMap>` to answer "is something already
//! happening to this node."

use std::collections::BTreeMap;

use crate::action::{Action, ActionRejected};
use crate::state::{
    ContainerDesired, ContainerInfo, ContainerObserved, PendingAction, RunState, WorkspaceState,
};

pub(super) fn reduce(
    state: &WorkspaceState,
    action: Action,
) -> Result<WorkspaceState, ActionRejected> {
    match action {
        Action::RunPlanned { run_id, plan } => {
            // Turning a `RunSpec` into real `ContainerInfo`/`VolumeInfo`
            // entries requires resolving the `.fghj.yaml` graph and calling
            // Docker, which is real I/O a pure reducer can't do — this just
            // records the still-unfulfilled intent (`pending_create`);
            // `effects::docker::converge` picks it up and reports the
            // result back via `Action::RunCreateSettled`.
            //
            // A run already known under this id keeps its existing
            // `containers`/`volumes` rather than being wiped back to empty:
            // the common case (`POST /runs` with no explicit `run_id`,
            // targeting the shared default environment) is an idempotent
            // top-up of whatever's already running, and clobbering it here
            // would make already-live containers flicker to "gone" for as
            // long as convergence takes, even though nothing is actually
            // being torn down.
            let mut next = state.clone();
            let run = next.runs.entry(run_id.clone()).or_insert_with(|| RunState {
                run_id,
                network: String::new(),
                containers: BTreeMap::new(),
                volumes: BTreeMap::new(),
                sidecar_container_name: String::new(),
                sidecar_ip: None,
                pending_create: None,
                pending_teardown: false,
            });
            if run.pending_create.is_some() {
                return Err(ActionRejected::AlreadyInFlight);
            }
            run.pending_create = Some(plan);
            Ok(next)
        }

        Action::RunStopRequested { run_id } => {
            let mut next = state.clone();
            let run = next
                .runs
                .get_mut(&run_id)
                .ok_or(ActionRejected::RunNotFound)?;
            // The run-level intent is what `effects::docker::converge`
            // acts on: tearing down the network, the sidecar and (for a
            // named run) the volumes has no per-container representation,
            // so marking containers alone would orphan all three.
            run.pending_teardown = true;
            // Containers are still marked so the UI shows them stopping
            // rather than sitting at "running" until the whole teardown
            // lands. Best-effort per container, not atomic across the run:
            // a node someone is already acting on is left alone rather than
            // failing the request, since making this all-or-nothing would
            // let one in-flight node block tearing the run down at all.
            for container in run.containers.values_mut() {
                if container.pending_action.is_none() {
                    container.desired.running = false;
                    container.pending_action = Some(PendingAction::Stopping);
                }
            }
            Ok(next)
        }

        Action::RunNodeStartRequested { run_id, node_id } => start_node(state, &run_id, &node_id),
        Action::RunNodeDebugWaitRequested {
            run_id,
            node_id,
            wait,
        } => set_debug_wait(state, &run_id, &node_id, wait),
        Action::RunNodeStopRequested { run_id, node_id } => {
            set_pending(state, &run_id, &node_id, PendingAction::Stopping, false)
        }
        Action::RunNodeDeleteRequested { run_id, node_id } => {
            // `desired.running` is left as-is: `Removing` is the signal an
            // effect needs to actually stop-then-remove the container,
            // and `reducer::observation::reduce` deletes it from
            // `containers` outright once that settles successfully — no
            // "desired to not exist" state is needed in between.
            let mut next = state.clone();
            let container = lookup_mut(&mut next, &run_id, &node_id)?;
            if container.pending_action.is_some() {
                return Err(ActionRejected::AlreadyInFlight);
            }
            container.pending_action = Some(PendingAction::Removing);
            Ok(next)
        }

        other => unreachable!("reducer::run::reduce called with a non-run action: {other:?}"),
    }
}

fn lookup_mut<'a>(
    state: &'a mut WorkspaceState,
    run_id: &str,
    node_id: &str,
) -> Result<&'a mut ContainerInfo, ActionRejected> {
    state
        .runs
        .get_mut(run_id)
        .ok_or(ActionRejected::RunNotFound)?
        .containers
        .get_mut(node_id)
        .ok_or(ActionRejected::NodeNotFound)
}

/// Flips the per-container debug switch (`FGHJ_DEBUG_WAIT`) and queues the
/// recreate that applies it.
///
/// There is deliberately no dedicated `PendingAction` for this: the switch
/// is a value in `desired`, and `PendingAction::Starting` is already the
/// convergence that makes a container match its desired state. So this
/// records the new desire and reuses that path — `converge::perform` reads
/// `desired.debug_wait` back out when it calls `restart_container`.
///
/// Unlike `start_node`, this requires the container to already exist. A node
/// with no container has nothing to halt, and inserting a placeholder would
/// mean "start this node, and also halt it before line 0" — two intents for
/// one switch. Start the node first.
fn set_debug_wait(
    state: &WorkspaceState,
    run_id: &str,
    node_id: &str,
    wait: bool,
) -> Result<WorkspaceState, ActionRejected> {
    let mut next = state.clone();
    let run = next
        .runs
        .get_mut(run_id)
        .ok_or(ActionRejected::RunNotFound)?;
    let container = run
        .containers
        .get_mut(node_id)
        .ok_or(ActionRejected::NodeNotFound)?;
    if container.pending_action.is_some() {
        return Err(ActionRejected::AlreadyInFlight);
    }
    container.desired.debug_wait = wait;
    container.desired.running = true;
    container.pending_action = Some(PendingAction::Starting);
    Ok(next)
}

/// `RunNodeStartRequested` is the one per-node action that must succeed even
/// when `node_id` has no live entry in `run.containers` — the Drawer's
/// "Start"/"Recreate" button is the documented way to bring a *deleted*
/// node's container back (`runs::RunRegistry::restart_container` looks the
/// node up in the resolved `.fghj.yaml` graph, not in any live container
/// list, so a prior `RunNodeDeleteRequested` removing the entry outright
/// must not make this node permanently unstartable). A fresh placeholder is
/// inserted here with just enough shape to carry `pending_action`;
/// `effects::docker::converge`'s `Starting` handling re-resolves the graph
/// and calls `restart_container` regardless of what's in `desired`, and
/// `Action::ContainerActionSettled` replaces this placeholder wholesale once
/// that call reports back — so nothing here needs to be a real value, only
/// present.
fn start_node(
    state: &WorkspaceState,
    run_id: &str,
    node_id: &str,
) -> Result<WorkspaceState, ActionRejected> {
    let mut next = state.clone();
    let run = next
        .runs
        .get_mut(run_id)
        .ok_or(ActionRejected::RunNotFound)?;
    match run.containers.get_mut(node_id) {
        Some(container) => {
            if container.pending_action.is_some() {
                return Err(ActionRejected::AlreadyInFlight);
            }
            container.desired.running = true;
            container.pending_action = Some(PendingAction::Starting);
        }
        None => {
            run.containers.insert(
                node_id.to_string(),
                ContainerInfo {
                    node_id: node_id.to_string(),
                    desired: ContainerDesired {
                        running: true,
                        container_name: String::new(),
                        domain: String::new(),
                        raw_domain: String::new(),
                        routes: Vec::new(),
                        additional_hosts: Vec::new(),
                        status_port: None,
                        config_hash: String::new(),
                        // Nothing has been built yet, so there is no
                        // checkout this container came from — and unlike the
                        // empty strings above, `None` is the value this
                        // field would legitimately hold forever for a
                        // non-built node, so it is not misread as resolved.
                        source: None,
                        // A placeholder for a node nothing has resolved yet
                        // — `start_node` overwrites the whole `desired` from
                        // the real graph when it reports back, and that is
                        // the only place `terminating` is ever decided.
                        terminating: false,
                        debug_wait: false,
                    },
                    observed: ContainerObserved::default(),
                    pending_action: Some(PendingAction::Starting),
                },
            );
        }
    }
    Ok(next)
}

fn set_pending(
    state: &WorkspaceState,
    run_id: &str,
    node_id: &str,
    pending: PendingAction,
    desired_running: bool,
) -> Result<WorkspaceState, ActionRejected> {
    let mut next = state.clone();
    let container = lookup_mut(&mut next, run_id, node_id)?;
    if container.pending_action.is_some() {
        return Err(ActionRejected::AlreadyInFlight);
    }
    container.desired.running = desired_running;
    container.pending_action = Some(pending);
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ContainerDesired, ContainerObserved};

    fn container(node_id: &str) -> ContainerInfo {
        ContainerInfo {
            node_id: node_id.into(),
            desired: ContainerDesired {
                running: false,
                container_name: format!("fghj-{node_id}-1"),
                domain: format!("{node_id}.fghj.internal"),
                raw_domain: format!("{node_id}.fghj.raw.internal"),
                routes: vec![],
                additional_hosts: vec![],
                status_port: None,
                config_hash: "hash".into(),
                source: None,
                terminating: false,
                debug_wait: false,
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
                pending_teardown: false,
            },
        );
        state
    }

    /// The switch is a value in `desired`, applied by the ordinary
    /// `Starting` convergence — so flipping it on must leave exactly that
    /// behind for `converge::perform` to read back out.
    #[test]
    fn debug_wait_requested_sets_the_desire_and_queues_the_recreate() {
        let state = state_with_run("default", vec![container("api")]);

        let next = reduce(
            &state,
            Action::RunNodeDebugWaitRequested {
                run_id: "default".into(),
                node_id: "api".into(),
                wait: true,
            },
        )
        .unwrap();

        let c = next.runs["default"].containers["api"].clone();
        assert!(c.desired.debug_wait);
        // The container has to come back up for the flag to mean anything:
        // a halted-at-startup container is still a *running* container.
        assert!(c.desired.running);
        assert_eq!(c.pending_action, Some(PendingAction::Starting));
    }

    #[test]
    fn debug_wait_can_be_turned_back_off() {
        let mut state = state_with_run("default", vec![container("api")]);
        state
            .runs
            .get_mut("default")
            .unwrap()
            .containers
            .get_mut("api")
            .unwrap()
            .desired
            .debug_wait = true;

        let next = reduce(
            &state,
            Action::RunNodeDebugWaitRequested {
                run_id: "default".into(),
                node_id: "api".into(),
                wait: false,
            },
        )
        .unwrap();

        assert!(!next.runs["default"].containers["api"].desired.debug_wait);
        assert_eq!(
            next.runs["default"].containers["api"].pending_action,
            Some(PendingAction::Starting)
        );
    }

    /// Unlike `start_node`, this does *not* insert a placeholder: a node with
    /// no container has nothing to halt, and conjuring one would fold two
    /// intents ("start this" and "halt it before line 0") into one switch.
    #[test]
    fn debug_wait_is_rejected_for_a_node_with_no_container() {
        let state = state_with_run("default", vec![container("api")]);

        let err = reduce(
            &state,
            Action::RunNodeDebugWaitRequested {
                run_id: "default".into(),
                node_id: "worker".into(),
                wait: true,
            },
        )
        .expect_err("no container means nothing to halt");

        assert_eq!(err, ActionRejected::NodeNotFound);
    }

    #[test]
    fn debug_wait_is_rejected_while_another_action_is_in_flight() {
        let mut state = state_with_run("default", vec![container("api")]);
        state
            .runs
            .get_mut("default")
            .unwrap()
            .containers
            .get_mut("api")
            .unwrap()
            .pending_action = Some(PendingAction::Stopping);

        let err = reduce(
            &state,
            Action::RunNodeDebugWaitRequested {
                run_id: "default".into(),
                node_id: "api".into(),
                wait: true,
            },
        )
        .expect_err("the switch must not race a stop");

        assert_eq!(err, ActionRejected::AlreadyInFlight);
    }

    #[test]
    fn run_planned_creates_an_empty_run_with_pending_create_set() {
        let state = WorkspaceState::default();
        let plan = crate::state::RunSpec {
            run_id: None,
            flow: None,
        };
        let next = reduce(
            &state,
            Action::RunPlanned {
                run_id: "default".into(),
                plan: plan.clone(),
            },
        )
        .unwrap();
        let run = next.runs.get("default").unwrap();
        assert!(run.containers.is_empty());
        assert_eq!(run.pending_create, Some(plan));
    }

    #[test]
    fn run_planned_preserves_an_existing_runs_containers() {
        let state = state_with_run("default", vec![container("web")]);
        let next = reduce(
            &state,
            Action::RunPlanned {
                run_id: "default".into(),
                plan: crate::state::RunSpec {
                    run_id: None,
                    flow: None,
                },
            },
        )
        .unwrap();
        let run = next.runs.get("default").unwrap();
        assert!(run.containers.contains_key("web"));
        assert!(run.pending_create.is_some());
    }

    #[test]
    fn run_planned_rejects_when_a_creation_is_already_in_flight() {
        let state = state_with_run("default", vec![container("web")]);
        let first = reduce(
            &state,
            Action::RunPlanned {
                run_id: "default".into(),
                plan: crate::state::RunSpec {
                    run_id: None,
                    flow: None,
                },
            },
        )
        .unwrap();
        let err = reduce(
            &first,
            Action::RunPlanned {
                run_id: "default".into(),
                plan: crate::state::RunSpec {
                    run_id: None,
                    flow: None,
                },
            },
        )
        .unwrap_err();
        assert_eq!(err, ActionRejected::AlreadyInFlight);
    }

    #[test]
    fn node_start_requested_sets_pending_and_desired_running() {
        let state = state_with_run("default", vec![container("web")]);
        let next = reduce(
            &state,
            Action::RunNodeStartRequested {
                run_id: "default".into(),
                node_id: "web".into(),
            },
        )
        .unwrap();
        let c = &next.runs["default"].containers["web"];
        assert_eq!(c.pending_action, Some(PendingAction::Starting));
        assert!(c.desired.running);
    }

    #[test]
    fn node_start_requested_rejects_when_already_in_flight() {
        let mut web = container("web");
        web.pending_action = Some(PendingAction::Starting);
        let state = state_with_run("default", vec![web]);
        let err = reduce(
            &state,
            Action::RunNodeStartRequested {
                run_id: "default".into(),
                node_id: "web".into(),
            },
        )
        .unwrap_err();
        assert_eq!(err, ActionRejected::AlreadyInFlight);
    }

    #[test]
    fn node_start_requested_rejects_unknown_run() {
        let state = WorkspaceState::default();
        let err = reduce(
            &state,
            Action::RunNodeStartRequested {
                run_id: "missing".into(),
                node_id: "web".into(),
            },
        )
        .unwrap_err();
        assert_eq!(err, ActionRejected::RunNotFound);
    }

    #[test]
    fn node_start_requested_inserts_a_placeholder_for_a_node_with_no_live_entry() {
        // Covers deleting a container then starting it again: `Removing`
        // settling removes the node's `ContainerInfo` outright
        // (`reducer::observation`), so this is the only remaining evidence
        // the node ever existed. It must still be startable — the Drawer's
        // "Start"/"Recreate" button is the documented way to bring a
        // deleted node back, and `restart_container` recreates from the
        // resolved `.fghj.yaml` graph regardless of what's in state.
        let state = state_with_run("default", vec![container("web")]);
        let next = reduce(
            &state,
            Action::RunNodeStartRequested {
                run_id: "default".into(),
                node_id: "missing".into(),
            },
        )
        .unwrap();
        let c = &next.runs["default"].containers["missing"];
        assert_eq!(c.pending_action, Some(PendingAction::Starting));
        assert!(c.desired.running);
    }

    #[test]
    fn node_stop_requested_sets_pending_and_clears_desired_running() {
        let mut web = container("web");
        web.desired.running = true;
        let state = state_with_run("default", vec![web]);
        let next = reduce(
            &state,
            Action::RunNodeStopRequested {
                run_id: "default".into(),
                node_id: "web".into(),
            },
        )
        .unwrap();
        let c = &next.runs["default"].containers["web"];
        assert_eq!(c.pending_action, Some(PendingAction::Stopping));
        assert!(!c.desired.running);
    }

    #[test]
    fn node_delete_requested_sets_removing_without_touching_desired() {
        let mut web = container("web");
        web.desired.running = true;
        let state = state_with_run("default", vec![web]);
        let next = reduce(
            &state,
            Action::RunNodeDeleteRequested {
                run_id: "default".into(),
                node_id: "web".into(),
            },
        )
        .unwrap();
        let c = &next.runs["default"].containers["web"];
        assert_eq!(c.pending_action, Some(PendingAction::Removing));
        assert!(c.desired.running);
    }

    #[test]
    fn run_stop_requested_stops_idle_containers_but_skips_in_flight_ones() {
        let mut web = container("web");
        web.desired.running = true;
        let mut api = container("api");
        api.desired.running = true;
        api.pending_action = Some(PendingAction::Starting);
        let state = state_with_run("default", vec![web, api]);

        let next = reduce(
            &state,
            Action::RunStopRequested {
                run_id: "default".into(),
            },
        )
        .unwrap();

        let run = &next.runs["default"];
        assert_eq!(
            run.containers["web"].pending_action,
            Some(PendingAction::Stopping)
        );
        assert!(!run.containers["web"].desired.running);
        // Untouched: it was already in flight.
        assert_eq!(
            run.containers["api"].pending_action,
            Some(PendingAction::Starting)
        );
        assert!(run.containers["api"].desired.running);
    }

    #[test]
    fn run_stop_requested_rejects_unknown_run() {
        let state = WorkspaceState::default();
        let err = reduce(
            &state,
            Action::RunStopRequested {
                run_id: "missing".into(),
            },
        )
        .unwrap_err();
        assert_eq!(err, ActionRejected::RunNotFound);
    }
}
