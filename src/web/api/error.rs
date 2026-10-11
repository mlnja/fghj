use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub(crate) fn err_response(e: anyhow::Error) -> Response {
    // `{e:?}` prints anyhow's whole cause chain; `Display` keeps only the outermost.
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

/// `AlreadyInFlight` -> 409; `NoEnvironment`/`NodeNotFound` -> 404.
pub(crate) fn action_rejected_response(err: crate::action::ActionRejected) -> Response {
    use crate::action::ActionRejected;
    let status = match err {
        ActionRejected::AlreadyInFlight => StatusCode::CONFLICT,
        ActionRejected::NoEnvironment | ActionRejected::NodeNotFound => StatusCode::NOT_FOUND,
    };
    (
        status,
        Json(serde_json::json!({ "error": err.to_string() })),
    )
        .into_response()
}
