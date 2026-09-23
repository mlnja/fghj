//! Starting, stopping and inspecting runs and the nodes inside them.

use axum::Json;
use axum::body::Bytes;
use axum::extract::Path as AxumPath;
use axum::response::{IntoResponse, Response};

use crate::web::api::error::{action_rejected_response, bad_request, err_response};
use crate::web::api::extract::{ActorExtractor, WorkspaceExtractor};
use crate::{action, actor, runs, state};

/// Builds the 200 response for a successful node-lifecycle dispatch: the
/// freshly-published `state::ContainerInfo` for `run_id`/`node_id`,
/// serialized as-is. There is exactly one container type in the codebase
/// now, so every endpoint that returns a container returns the same
/// `{node_id, desired, observed, pending_action}` shape without a view
/// layer in between. Falls back to a bare `{"ok": true}` in the
/// (practically unreachable, since `reduce` always leaves a container that
/// hasn't been removed by `ContainerActionSettled` in place) case the
/// container isn't found right after a successful dispatch.
pub(crate) fn container_response(
    actor: &actor::ActorHandle,
    run_id: &str,
    node_id: &str,
) -> Response {
    let current = actor.current();
    match current
        .runs
        .get(run_id)
        .and_then(|run| run.containers.get(node_id))
    {
        Some(container) => Json(container).into_response(),
        None => Json(serde_json::json!({ "ok": true })).into_response(),
    }
}

/// Every run the reducer knows about, as a JSON array — the UI picks runs
/// out of it by `run_id`, so the order the `BTreeMap` iterates in (by id)
/// is the whole contract.
pub(crate) async fn get_runs(ActorExtractor(actor): ActorExtractor) -> Response {
    let current = actor.current();
    let runs: Vec<&state::RunState> = current.runs.values().collect();
    Json(runs).into_response()
}

/// Builds the response for a successful `RunPlanned` dispatch: whatever
/// `run_id` names in the actor's freshly-published state, per the same
/// "respond once the reducer has recorded intent, not once Docker has
/// actually finished" contract `dispatch_node_action` already uses (see the
/// architecture plan's "HTTP handler contract"). `RunPlanned`'s reducer arm
/// always inserts an entry under `run_id` (empty on a brand new run,
/// top-up-preserved on an existing one), so the `None` branch is
/// practically unreachable — kept only for symmetry with
/// `container_response`.
pub(crate) fn run_response(actor: &actor::ActorHandle, run_id: &str) -> Response {
    match actor.current().runs.get(run_id) {
        Some(run) => Json(run).into_response(),
        None => Json(serde_json::json!({ "ok": true, "run_id": run_id })).into_response(),
    }
}

/// Dispatches `Action::RunPlanned` through the workspace actor instead of
/// calling `runs::RunRegistry::start`/`ensure_running` directly — migration
/// phase 5's HTTP cutover. Unlike the old synchronous handler, this returns
/// as soon as the reducer has recorded the still-unfulfilled intent
/// (`RunState::pending_create`); `effects::docker::converge`'s
/// `DockerConvergeEffect` (already wired per-workspace, see
/// `WorkspaceRegistry::wire_actor`) is what actually resolves the graph and
/// calls Docker afterwards, reporting the result back via
/// `Action::RunCreateSettled`. This is the same async contract migration
/// phase 4 already gave node-lifecycle endpoints — run creation was the one
/// endpoint still on the old fully-synchronous path, purely because of a
/// JSON-shape mismatch between the two container models that collapsing to
/// a single model removed, not because of anything about creation itself
/// that needed different timing.
///
/// A freshly-created run's first response (and the `GET /runs` polls
/// immediately after it) can therefore show 0 containers for as long as
/// convergence takes, where the old handler always returned the fully
/// populated result — the same "trust `pending_action`/poll for the rest"
/// model the UI already applies to node start/stop/delete, just not
/// something it has a "run is being created" affordance for yet
/// (`pending_create` is deliberately never serialized — see its doc).
pub(crate) async fn post_runs(ActorExtractor(actor): ActorExtractor, body: Bytes) -> Response {
    let spec: state::RunSpec = if body.is_empty() {
        state::RunSpec {
            run_id: None,
            flow: None,
        }
    } else {
        match serde_json::from_slice(&body) {
            Ok(s) => s,
            Err(e) => return bad_request(e),
        }
    };

    let run_id = runs::resolve_run_id(spec.run_id.as_deref());
    let action = action::Action::RunPlanned {
        run_id: run_id.clone(),
        plan: spec,
    };
    match actor.dispatch(action).await {
        Ok(()) => run_response(&actor, &run_id),
        Err(e) => action_rejected_response(e),
    }
}

/// Deliberately left on the old `WorkspaceExtractor` / `RunRegistry::stop`
/// path, unlike the three per-node handlers below — migration phase 4
/// ("HTTP handler contract" in the architecture plan) only names
/// `/nodes/{node}/start|stop|delete`, never whole-run stop, and for good
/// reason: `RunRegistry::stop` tears down the run's network, sidecar and
/// volumes and drops its `RunRegistry` entry outright, none of which the
/// `pending_action`-per-container model that `effects::docker::converge`
/// converges has any representation for. `Action::RunStopRequested`'s
/// reducer arm only marks each idle container `Stopping`; routing this
/// endpoint through it would leave the network/sidecar/volumes orphaned.
/// Giving whole-run teardown its own first-class action/effect is later
/// migration-phase work, not something to half-do here.
pub(crate) async fn post_run_stop(
    AxumPath(run_id): AxumPath<String>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state.runs.stop(&run_id).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => err_response(e),
    }
}

/// Dispatches `action` against `actor` for `run_id`/`node_id` and replies
/// per the architecture plan's "HTTP handler contract" — migration phase 4:
/// this returns as soon as the (pure, in-memory) reducer has recorded the
/// intent, not once Docker has actually finished; `effects::docker::converge`
/// (wired per-workspace in `daemon::WorkspaceRegistry::wire_actor`) is what
/// actually performs the Docker call afterwards, asynchronously.
pub(crate) async fn dispatch_node_action(
    actor: &actor::ActorHandle,
    run_id: String,
    node_id: String,
    action: action::Action,
) -> Response {
    match actor.dispatch(action).await {
        Ok(()) => container_response(actor, &run_id, &node_id),
        Err(e) => action_rejected_response(e),
    }
}

pub(crate) async fn post_run_node_start(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    ActorExtractor(actor): ActorExtractor,
) -> Response {
    let action = action::Action::RunNodeStartRequested {
        run_id: run_id.clone(),
        node_id: node_id.clone(),
    };
    dispatch_node_action(&actor, run_id, node_id, action).await
}

pub(crate) async fn post_run_node_stop(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    ActorExtractor(actor): ActorExtractor,
) -> Response {
    let action = action::Action::RunNodeStopRequested {
        run_id: run_id.clone(),
        node_id: node_id.clone(),
    };
    dispatch_node_action(&actor, run_id, node_id, action).await
}

pub(crate) async fn post_run_node_delete(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    ActorExtractor(actor): ActorExtractor,
) -> Response {
    let action = action::Action::RunNodeDeleteRequested {
        run_id: run_id.clone(),
        node_id: node_id.clone(),
    };
    dispatch_node_action(&actor, run_id, node_id, action).await
}
