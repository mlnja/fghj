//! Kicking off and polling background clone/pull jobs.

use axum::Json;
use axum::extract::{Path as AxumPath, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::downloads;
use crate::web::api::extract::WorkspaceExtractor;

#[derive(Deserialize)]
pub(crate) struct FlowQuery {
    flow: Option<String>,
}

pub(crate) async fn post_pull_all(
    Query(q): Query<FlowQuery>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    let owner = state.db.clone().load_owner().await.ok().flatten();
    let s = state
        .downloads
        .start_pull_all(state.path.clone(), owner, q.flow);
    Json(serde_json::json!(s)).into_response()
}

pub(crate) async fn get_pull_all_status(
    Query(q): Query<FlowQuery>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    let key = downloads::pull_all_key(q.flow.as_deref());
    match state.downloads.status(&key) {
        Some(s) => Json(serde_json::json!(s)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no pull-all job has been started" })),
        )
            .into_response(),
    }
}

pub(crate) async fn post_pull_node(
    AxumPath(node_id): AxumPath<String>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    let owner = state.db.clone().load_owner().await.ok().flatten();
    let s = state
        .downloads
        .start_node(state.path.clone(), node_id, owner);
    Json(serde_json::json!(s)).into_response()
}

pub(crate) async fn get_pull_node_status(
    AxumPath(node_id): AxumPath<String>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state.downloads.status(&format!("node:{node_id}")) {
        Some(s) => Json(serde_json::json!(s)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no download job for {node_id}") })),
        )
            .into_response(),
    }
}

/// Lists every pull/download job the workspace has ever started, most-recent
/// first — backs the UI's single "operations queue" drawer.
pub(crate) async fn get_pull_jobs(WorkspaceExtractor(state): WorkspaceExtractor) -> Response {
    Json(serde_json::json!(state.downloads.list())).into_response()
}
