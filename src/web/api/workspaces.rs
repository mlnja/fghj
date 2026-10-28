use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::daemon::registry::WorkspaceRegistry;
use crate::web::api::error::{bad_request, err_response};
use crate::web::api::extract::WorkspaceExtractor;
use crate::{persistence, resolver};

#[derive(Deserialize)]
pub(crate) struct StartRequest {
    entry: Option<String>,
    workspace: Option<PathBuf>,
    #[serde(default)]
    owner: Option<persistence::WorkspaceOwner>,
}

#[derive(Deserialize)]
pub(crate) struct RemoveRequest {
    id: String,
}
pub(crate) async fn get_workspaces(State(registry): State<Arc<WorkspaceRegistry>>) -> Response {
    let list: Vec<_> = registry
        .list()
        .into_iter()
        .map(|(id, workspace)| serde_json::json!({ "id": id, "workspace": workspace }))
        .collect();
    Json(serde_json::json!(list)).into_response()
}

pub(crate) async fn post_workspaces(
    State(registry): State<Arc<WorkspaceRegistry>>,
    body: Bytes,
) -> Response {
    match serde_json::from_slice::<StartRequest>(&body) {
        Ok(req) => match registry.resolve(req.entry, req.workspace, req.owner).await {
            Ok((id, workspace)) => {
                // Before answering, so `fghj wire` returns with the browsers
                // already trusting the CA, or says why they don't.
                let mut warnings = Vec::new();
                if let Some(state) = registry.get(&id)
                    && let Some(owner) =
                        persistence::workspace_owner(state.db.clone(), &state.path).await
                {
                    warnings = crate::daemon::browser_trust::trust_for(vec![owner]).await;
                }
                Json(serde_json::json!({ "id": id, "workspace": workspace, "warnings": warnings }))
                    .into_response()
            }
            Err(e) => err_response(e),
        },
        Err(e) => bad_request(e),
    }
}

/// 202 with the removal job: the workspace is gone from `/workspaces` at
/// once, its containers and network go in the background (`/activities`).
pub(crate) async fn post_workspaces_remove(
    State(registry): State<Arc<WorkspaceRegistry>>,
    body: Bytes,
) -> Response {
    match serde_json::from_slice::<RemoveRequest>(&body) {
        Ok(req) => match registry.remove(&req.id) {
            Ok(Some(job)) => (StatusCode::ACCEPTED, Json(serde_json::json!(job))).into_response(),
            Ok(None) => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("no workspace {}", req.id) })),
            )
                .into_response(),
            Err(e) => err_response(e),
        },
        Err(e) => bad_request(e),
    }
}

pub(crate) async fn get_universe(WorkspaceExtractor(state): WorkspaceExtractor) -> Response {
    match resolver::resolve_universe_async(state.path.clone()).await {
        Ok(g) => Json(serde_json::json!(g)).into_response(),
        Err(e) => err_response(e),
    }
}
