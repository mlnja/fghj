use std::convert::Infallible;

use axum::Json;
use axum::extract::{Path as AxumPath, Query};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Deserialize;

use crate::web::api::error::{bad_request, err_response};
use crate::web::api::extract::{ActorExtractor, WorkspaceExtractor};
use crate::{docker, orchestrator};

#[derive(Deserialize)]
pub(crate) struct TailQuery {
    tail: Option<usize>,
}

pub(crate) async fn get_node_logs(
    AxumPath(node_id): AxumPath<String>,
    Query(q): Query<TailQuery>,
    WorkspaceExtractor(state): WorkspaceExtractor,
    ActorExtractor(actor): ActorExtractor,
) -> Response {
    let tail = q.tail.unwrap_or(200);
    match actor.current().env.as_ref() {
        Some(s) => match orchestrator::logs_for_tail(&state.docker, s, &node_id, tail).await {
            Ok(text) => Json(serde_json::json!({ "logs": text })).into_response(),
            Err(e) => err_response(e),
        },
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no environment yet" })),
        )
            .into_response(),
    }
}

pub(crate) async fn get_node_logs_stream(
    AxumPath(node_id): AxumPath<String>,
    WorkspaceExtractor(state): WorkspaceExtractor,
    ActorExtractor(actor): ActorExtractor,
) -> Response {
    let run_state = match actor.current().env.clone() {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "no environment yet" })),
            )
                .into_response();
        }
    };
    let container_name = match orchestrator::container_name_for(&run_state, &node_id) {
        Ok(name) => name.to_string(),
        Err(e) => return bad_request(e),
    };

    let stream = docker::logs_follow(&state.docker, &container_name, false).map(|item| {
        let event = match item {
            Ok(chunk) => Event::default().data(chunk.to_string()),
            Err(e) => Event::default().event("error").data(e.to_string()),
        };
        Ok::<Event, Infallible>(event)
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Reads the db directly, so a removed container's history stays browsable.
pub(crate) async fn get_run_node_log_generations(
    AxumPath(node_id): AxumPath<String>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state.db.clone().list_log_generations(node_id).await {
        Ok(generations) => Json(serde_json::json!({ "generations": generations })).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Deserialize)]
pub(crate) struct LogHistoryQuery {
    generation: i64,
    before_seq: Option<i64>,
    #[serde(default = "default_log_history_limit")]
    limit: i64,
}

pub(crate) fn default_log_history_limit() -> i64 {
    500
}

/// Oldest first. `before_seq` pages backwards.
pub(crate) async fn get_run_node_log_history(
    AxumPath(node_id): AxumPath<String>,
    Query(q): Query<LogHistoryQuery>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state
        .db
        .clone()
        .load_log_history(node_id, q.generation, q.before_seq, q.limit)
        .await
    {
        Ok(lines) => Json(serde_json::json!({ "lines": lines })).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Deserialize)]
pub(crate) struct EventsQuery {
    action: String,
}

/// fghjd's own narration of the last `start` or `stop` of this node.
pub(crate) async fn get_node_events(
    AxumPath(node_id): AxumPath<String>,
    Query(q): Query<EventsQuery>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state.db.clone().list_events(node_id, q.action).await {
        Ok(events) => Json(serde_json::json!({ "events": events })).into_response(),
        Err(e) => err_response(e),
    }
}
