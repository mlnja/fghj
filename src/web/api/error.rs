//! Turning an internal failure into the HTTP response shape the UI and CLI expect.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub(crate) fn err_response(e: anyhow::Error) -> Response {
    // `e.to_string()` (anyhow's `Display`) only prints the outermost
    // `.context(...)` layer — e.g. just "docker run <name> failed" with the
    // actual Docker Engine API error message it wraps discarded. `{e:?}`
    // (anyhow's `Debug`) prints the full "Caused by:" chain instead, which is
    // the only version that's actually diagnosable from the API response.
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": format!("{e:?}") })),
    )
        .into_response()
}

pub(crate) fn bad_request(e: impl std::fmt::Display) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": e.to_string() })),
    )
        .into_response()
}

/// Maps an `ActionRejected` from a dispatched node-lifecycle request to its
/// HTTP response — the migration-phase-4 counterpart of `err_response` for
/// handlers on the new `actor::ActorHandle::dispatch` path.
/// `AlreadyInFlight` -> 409, matching today's `RunRegistry::begin_action`
/// rejection exactly (see the architecture plan's "HTTP handler contract").
/// `RunNotFound`/`NodeNotFound` -> 404: an improvement over the old path's
/// blanket 500 (`bail!("no such run: ..")` via `err_response`), now that
/// the reducer distinguishes the two cases explicitly.
pub(crate) fn action_rejected_response(err: crate::action::ActionRejected) -> Response {
    use crate::action::ActionRejected;
    let status = match err {
        ActionRejected::AlreadyInFlight => StatusCode::CONFLICT,
        ActionRejected::RunNotFound | ActionRejected::NodeNotFound => StatusCode::NOT_FOUND,
    };
    (
        status,
        Json(serde_json::json!({ "error": err.to_string() })),
    )
        .into_response()
}
