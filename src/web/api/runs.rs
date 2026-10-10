//! Starting, stopping and inspecting the environment and the nodes in it.

use axum::Json;
use axum::body::Bytes;
use axum::extract::Path as AxumPath;
use axum::response::{IntoResponse, Response};

use crate::web::api::error::{action_rejected_response, bad_request};
use crate::web::api::extract::ActorExtractor;
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

/// The workspace's environment — every container fghj knows about — or
/// `null` before anything has been started.
pub(crate) async fn get_environment(ActorExtractor(actor): ActorExtractor) -> Response {
    Json(actor.current().runs.get(runs::DEFAULT_RUN_ID)).into_response()
}

/// Switches the environment to a flow: body `{"flow": "repo/flow"}`. Starts
/// the flow's nodes and stops (not removes) every running container outside
/// it. Returns once the intent is recorded; `effects::docker::converge` does
/// the Docker work and reports back via `Action::RunCreateSettled`.
pub(crate) async fn post_switch(ActorExtractor(actor): ActorExtractor, body: Bytes) -> Response {
    #[derive(serde::Deserialize)]
    struct Body {
        flow: String,
    }
    let body: Body = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return bad_request(e),
    };
    let action = action::Action::RunPlanned {
        run_id: runs::DEFAULT_RUN_ID.to_string(),
        plan: state::RunSpec::Flow(body.flow),
    };
    match actor.dispatch(action).await {
        Ok(()) => Json(actor.current().runs.get(runs::DEFAULT_RUN_ID)).into_response(),
        Err(e) => action_rejected_response(e),
    }
}

/// Tears the whole environment down — containers, network and sidecar.
/// Volumes are kept. Same dispatch-and-return contract as every other
/// mutating handler: the reducer records the intent
/// (`RunState::pending_teardown`) and `effects::docker::converge` performs
/// it, dropping the run from state via `Action::RunTeardownSettled`.
pub(crate) async fn post_stop(ActorExtractor(actor): ActorExtractor) -> Response {
    match actor
        .dispatch(action::Action::RunStopRequested {
            run_id: runs::DEFAULT_RUN_ID.to_string(),
        })
        .await
    {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => action_rejected_response(e),
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
    AxumPath(node_id): AxumPath<String>,
    ActorExtractor(actor): ActorExtractor,
) -> Response {
    let run_id = runs::DEFAULT_RUN_ID.to_string();
    let action = action::Action::RunNodeStartRequested {
        run_id: run_id.clone(),
        node_id: node_id.clone(),
    };
    dispatch_node_action(&actor, run_id, node_id, action).await
}

/// The per-container debug switch (`FGHJ_DEBUG_WAIT`) — see
/// `state::ContainerDesired::debug_wait`. Body is `{"wait": true|false}`.
///
/// There is no dedicated pending action: the reducer records the new desire
/// and queues `PendingAction::Starting`, and the ordinary convergence reads
/// `desired.debug_wait` back out when it recreates the container. Requires
/// the container to already exist — a node with nothing running has nothing
/// to halt.
pub(crate) async fn post_run_node_debug_wait(
    AxumPath(node_id): AxumPath<String>,
    ActorExtractor(actor): ActorExtractor,
    body: Bytes,
) -> Response {
    let run_id = runs::DEFAULT_RUN_ID.to_string();
    #[derive(serde::Deserialize)]
    struct Body {
        wait: bool,
    }
    // An *empty* body means on, matching the bare-POST convention of
    // `/start` and `/stop` — the request itself is the intent. A body that
    // is present but unparseable is a 400 rather than a default, because
    // "turn it on" is the direction that halts a container and stalls
    // everything downstream of it; a caller that meant `{"wait": false}`
    // and typo'd it must not be told the opposite happened.
    let wait = if body.is_empty() {
        true
    } else {
        match serde_json::from_slice::<Body>(&body) {
            Ok(b) => b.wait,
            Err(e) => return bad_request(e),
        }
    };
    let action = action::Action::RunNodeDebugWaitRequested {
        run_id: run_id.clone(),
        node_id: node_id.clone(),
        wait,
    };
    dispatch_node_action(&actor, run_id, node_id, action).await
}

pub(crate) async fn post_run_node_stop(
    AxumPath(node_id): AxumPath<String>,
    ActorExtractor(actor): ActorExtractor,
) -> Response {
    let run_id = runs::DEFAULT_RUN_ID.to_string();
    let action = action::Action::RunNodeStopRequested {
        run_id: run_id.clone(),
        node_id: node_id.clone(),
    };
    dispatch_node_action(&actor, run_id, node_id, action).await
}

pub(crate) async fn post_run_node_delete(
    AxumPath(node_id): AxumPath<String>,
    ActorExtractor(actor): ActorExtractor,
) -> Response {
    let run_id = runs::DEFAULT_RUN_ID.to_string();
    let action = action::Action::RunNodeDeleteRequested {
        run_id: run_id.clone(),
        node_id: node_id.clone(),
    };
    dispatch_node_action(&actor, run_id, node_id, action).await
}
