//! Serving the embedded Svelte UI.

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

use crate::server::{self};

pub(crate) async fn static_handler(uri: Uri) -> Response {
    let (body, content_type, status) = server::static_response(uri.path());
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        [(header::CONTENT_TYPE, content_type)],
        body,
    )
        .into_response()
}
