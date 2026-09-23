//! Container logs: tail, live stream, generations, history, and events.

use std::convert::Infallible;

use axum::Json;
use axum::extract::{Path as AxumPath, Query};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Deserialize;

use crate::web::api::error::{bad_request, err_response};
use crate::web::api::extract::WorkspaceExtractor;
use crate::{docker, runs};

#[derive(Deserialize)]
pub(crate) struct TailQuery {
    tail: Option<usize>,
}

pub(crate) async fn get_run_logs(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    Query(q): Query<TailQuery>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    let tail = q.tail.unwrap_or(200);
    match state.runs.get(&run_id) {
        Some(s) => match runs::logs_for_tail(&state.docker, &s, &node_id, tail).await {
            Ok(text) => Json(serde_json::json!({ "logs": text })).into_response(),
            Err(e) => err_response(e),
        },
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no such run: {run_id}") })),
        )
            .into_response(),
    }
}

pub(crate) async fn get_run_logs_stream(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    let run_state = match state.runs.get(&run_id) {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("no such run: {run_id}") })),
            )
                .into_response();
        }
    };
    let container_name = match runs::container_name_for(&run_state, &node_id) {
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

/// Lists the log generations `fghjd` has retained for this node (current and
/// previous, per the two-generation retention policy — see
/// `persistence::WorkspaceDb::begin_log_generation`), for the UI's generation
/// picker. Reads straight from the db regardless of whether the run/node is
/// still live, so history for a just-crashed or just-removed container
/// stays browsable.
pub(crate) async fn get_run_node_log_generations(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state.db.clone().list_log_generations(run_id, node_id).await {
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

/// Paginated log history for one generation, oldest-first — backs both the
/// initial load (`before_seq` omitted, returns the most recent `limit`
/// lines) and infinite scroll-back (`before_seq` set to the oldest `seq`
/// currently loaded).
pub(crate) async fn get_run_node_log_history(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    Query(q): Query<LogHistoryQuery>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state
        .db
        .clone()
        .load_log_history(run_id, node_id, q.generation, q.before_seq, q.limit)
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

/// The current cycle's step-by-step narration of what `fghjd` itself did
/// for the last `start` or `stop` of this node — the ArgoCD-style "events"
/// counterpart to the raw container-log history above. Only ever the most
/// recent cycle of `action`: see `persistence::WorkspaceDb::begin_event_cycle`.
pub(crate) async fn get_run_node_events(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    Query(q): Query<EventsQuery>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state
        .db
        .clone()
        .list_events(run_id, node_id, q.action)
        .await
    {
        Ok(events) => Json(serde_json::json!({ "events": events })).into_response(),
        Err(e) => err_response(e),
    }
}
