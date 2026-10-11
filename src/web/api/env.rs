use axum::Json;
use axum::body::Bytes;
use axum::extract::Path as AxumPath;
use axum::response::{IntoResponse, Response};

use crate::web::api::error::{action_rejected_response, bad_request};
use crate::web::api::extract::ActorExtractor;
use crate::{action, actor, state};

/// Falls back to `{"ok": true}` if the container is gone already.
pub(crate) fn container_response(actor: &actor::ActorHandle, node_id: &str) -> Response {
    let current = actor.current();
    match current
        .env
        .as_ref()
        .and_then(|env| env.containers.get(node_id))
    {
        Some(container) => Json(container).into_response(),
        None => Json(serde_json::json!({ "ok": true })).into_response(),
    }
}

pub(crate) async fn get_environment(ActorExtractor(actor): ActorExtractor) -> Response {
    Json(&actor.current().env).into_response()
}

/// Switches to a flow: body `{"flow": "repo/flow"}`. Replies once the intent
/// is recorded; Docker work happens in `effects::docker::converge`.
pub(crate) async fn post_switch(ActorExtractor(actor): ActorExtractor, body: Bytes) -> Response {
    #[derive(serde::Deserialize)]
    struct Body {
        flow: String,
    }
    let body: Body = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return bad_request(e),
    };
    let action = action::Action::EnvPlanned {
        plan: state::EnvSpec::Flow(body.flow),
    };
    match actor.dispatch(action).await {
        Ok(()) => Json(&actor.current().env).into_response(),
        Err(e) => action_rejected_response(e),
    }
}

/// Replies once the reducer has recorded the intent, not when Docker is done.
pub(crate) async fn dispatch_node_action(
    actor: &actor::ActorHandle,
    node_id: String,
    action: action::Action,
) -> Response {
    match actor.dispatch(action).await {
        Ok(()) => container_response(actor, &node_id),
        Err(e) => action_rejected_response(e),
    }
}

pub(crate) async fn post_node_start(
    AxumPath(node_id): AxumPath<String>,
    ActorExtractor(actor): ActorExtractor,
) -> Response {
    let action = action::Action::NodeStartRequested {
        node_id: node_id.clone(),
    };
    dispatch_node_action(&actor, node_id, action).await
}

/// Body `{"wait": true|false}`. Requires an existing container.
pub(crate) async fn post_node_debug_wait(
    AxumPath(node_id): AxumPath<String>,
    ActorExtractor(actor): ActorExtractor,
    body: Bytes,
) -> Response {
    #[derive(serde::Deserialize)]
    struct Body {
        wait: bool,
    }
    // An empty body means on, like `/start`. An unparseable one is a 400, not a
    // default: "on" halts the container, so a typo mustn't turn it on.
    let wait = if body.is_empty() {
        true
    } else {
        match serde_json::from_slice::<Body>(&body) {
            Ok(b) => b.wait,
            Err(e) => return bad_request(e),
        }
    };
    let action = action::Action::NodeDebugWaitRequested {
        node_id: node_id.clone(),
        wait,
    };
    dispatch_node_action(&actor, node_id, action).await
}

pub(crate) async fn post_node_stop(
    AxumPath(node_id): AxumPath<String>,
    ActorExtractor(actor): ActorExtractor,
) -> Response {
    let action = action::Action::NodeStopRequested {
        node_id: node_id.clone(),
    };
    dispatch_node_action(&actor, node_id, action).await
}

pub(crate) async fn post_node_delete(
    AxumPath(node_id): AxumPath<String>,
    ActorExtractor(actor): ActorExtractor,
) -> Response {
    let action = action::Action::NodeDeleteRequested {
        node_id: node_id.clone(),
    };
    dispatch_node_action(&actor, node_id, action).await
}
