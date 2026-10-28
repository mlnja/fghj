use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::activity::{Activity, ActivityKind};
use crate::daemon::registry::WorkspaceRegistry;
use crate::web::api::extract::WorkspaceExtractor;
use crate::{downloads, persistence};

#[derive(Deserialize)]
pub(crate) struct FlowQuery {
    flow: Option<String>,
}

fn image_puller(registry: &WorkspaceRegistry) -> downloads::ImagePuller {
    downloads::ImagePuller {
        docker: registry.docker(),
        runtime: tokio::runtime::Handle::current(),
    }
}

pub(crate) async fn post_pull_all(
    State(registry): State<Arc<WorkspaceRegistry>>,
    Query(q): Query<FlowQuery>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    let owner = persistence::workspace_owner(state.db.clone(), &state.path).await;
    let s = downloads::start_pull_all(
        &state.activities,
        &state.id,
        state.path.clone(),
        owner,
        q.flow,
        image_puller(&registry),
    );
    Json(serde_json::json!(s)).into_response()
}

pub(crate) async fn get_pull_all_status(
    Query(q): Query<FlowQuery>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    let key = downloads::pull_all_key(q.flow.as_deref());
    match state
        .activities
        .get(Some(&state.id), ActivityKind::Pull, &key)
    {
        Some(s) => Json(serde_json::json!(s)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no pull-all job has been started" })),
        )
            .into_response(),
    }
}

pub(crate) async fn post_pull_node(
    State(registry): State<Arc<WorkspaceRegistry>>,
    AxumPath(node_id): AxumPath<String>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    let owner = persistence::workspace_owner(state.db.clone(), &state.path).await;
    let s = downloads::start_node(
        &state.activities,
        &state.id,
        state.path.clone(),
        node_id,
        owner,
        image_puller(&registry),
    );
    Json(serde_json::json!(s)).into_response()
}

pub(crate) async fn get_pull_node_status(
    AxumPath(node_id): AxumPath<String>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state.activities.get(
        Some(&state.id),
        ActivityKind::Pull,
        &format!("node:{node_id}"),
    ) {
        Some(s) => Json(serde_json::json!(s)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no download job for {node_id}") })),
        )
            .into_response(),
    }
}

/// Top-level pulls only; `/activities` has their clones.
pub(crate) async fn get_pull_jobs(WorkspaceExtractor(state): WorkspaceExtractor) -> Response {
    let jobs: Vec<Activity> = state
        .activities
        .list(Some(&state.id))
        .into_iter()
        .filter(|a| a.kind == ActivityKind::Pull && a.parent.is_none())
        .collect();
    Json(serde_json::json!(jobs)).into_response()
}

#[derive(Deserialize)]
pub(crate) struct ActivitiesQuery {
    workspace: Option<String>,
}

/// Every workspace's unless `?workspace=` narrows it, so a removed
/// workspace's jobs stay reachable.
pub(crate) async fn get_activities(
    Query(q): Query<ActivitiesQuery>,
    State(registry): State<Arc<WorkspaceRegistry>>,
) -> Response {
    Json(serde_json::json!(
        registry.activities().list(q.workspace.as_deref())
    ))
    .into_response()
}
