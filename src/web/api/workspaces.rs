//! Wiring, listing and stopping workspaces; serving the resolved universe.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
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
pub(crate) struct StopRequest {
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
                Json(serde_json::json!({ "id": id, "workspace": workspace })).into_response()
            }
            Err(e) => err_response(e),
        },
        Err(e) => bad_request(e),
    }
}

pub(crate) async fn post_workspaces_stop(
    State(registry): State<Arc<WorkspaceRegistry>>,
    body: Bytes,
) -> Response {
    match serde_json::from_slice::<StopRequest>(&body) {
        Ok(req) => {
            Json(serde_json::json!({ "stopped": registry.stop(&req.id).await })).into_response()
        }
        Err(e) => bad_request(e),
    }
}

pub(crate) async fn get_universe(WorkspaceExtractor(state): WorkspaceExtractor) -> Response {
    let path = state.path.clone();
    match tokio::task::spawn_blocking(move || resolver::resolve_universe(&path)).await {
        Ok(Ok(g)) => Json(serde_json::json!(g)).into_response(),
        Ok(Err(e)) => err_response(e),
        Err(e) => err_response(anyhow::anyhow!("resolve_universe task panicked: {e}")),
    }
}
