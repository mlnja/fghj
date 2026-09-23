//! Axum extractors that resolve `?workspace=<id>` to a live workspace.

use std::sync::Arc;

use anyhow::Result;
use axum::Json;
use axum::extract::FromRequestParts;
use axum::http::{StatusCode, request::Parts};
use axum::response::{IntoResponse, Response};

use crate::actor;
use crate::daemon::registry::WorkspaceRegistry;
use crate::server::WorkspaceState;
use crate::web::query::query_param;

/// Extracts the workspace named by `?workspace=<id>` in the request's query
/// string, or rejects with the same 400 the old handler used to return for a
/// missing/unknown id.
pub(crate) struct WorkspaceExtractor(pub(crate) Arc<WorkspaceState>);

impl FromRequestParts<Arc<WorkspaceRegistry>> for WorkspaceExtractor {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<WorkspaceRegistry>,
    ) -> Result<Self, Self::Rejection> {
        let query = parts.uri.query().unwrap_or("");
        match query_param(query, "workspace").and_then(|id| state.get(id)) {
            Some(ws) => Ok(WorkspaceExtractor(ws)),
            None => Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "unknown or missing ?workspace=<id>; POST /workspaces first" })),
            )
                .into_response()),
        }
    }
}

/// Extracts the new-system `actor::ActorHandle` for the workspace named by
/// `?workspace=<id>` — the migration-phase-4 counterpart of
/// `WorkspaceExtractor` for handlers that dispatch an `Action` instead of
/// calling `runs::RunRegistry` directly. Kept as its own extractor rather
/// than folded into `WorkspaceExtractor` (which every other handler still
/// uses unchanged) since only the three node-lifecycle handlers need it.
pub(crate) struct ActorExtractor(pub(crate) actor::ActorHandle);

impl FromRequestParts<Arc<WorkspaceRegistry>> for ActorExtractor {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<WorkspaceRegistry>,
    ) -> Result<Self, Self::Rejection> {
        let query = parts.uri.query().unwrap_or("");
        match query_param(query, "workspace").and_then(|id| state.actors().get(id)) {
            Some(handle) => Ok(ActorExtractor(handle.actor)),
            None => Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "unknown or missing ?workspace=<id>; POST /workspaces first" })),
            )
                .into_response()),
        }
    }
}
