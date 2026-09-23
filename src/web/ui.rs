//! Serving the embedded Svelte UI bundle.
//!
//! The bundle is compiled into the binary (`include_dir!`) rather than read
//! off disk at runtime, so `fghjd` has no "where did the UI go" failure mode
//! and no install step beyond the binary itself.

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use include_dir::{Dir, include_dir};

/// `pub(crate)` so `sidecar_image` can reuse this same embedded copy when
/// materializing the sidecar's Docker build context, rather than embedding
/// the UI a second time.
pub(crate) static UI_DIST: Dir = include_dir!("$CARGO_MANIFEST_DIR/ui/dist");

/// Serves the embedded UI bundle, falling back to `index.html` for
/// unmatched routes (SPA-style) — the frontend owns its own routing, so a
/// deep link the server has never heard of is a client route, not a 404.
///
/// Split from [`static_handler`] only so it's testable without spinning up
/// axum: it answers "what bytes and content type does this path get",
/// nothing about HTTP.
fn static_response(route: &str) -> (Vec<u8>, &'static str, u16) {
    let trimmed = route.trim_start_matches('/');
    let file_path = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };
    match UI_DIST.get_file(file_path) {
        Some(f) => (
            f.contents().to_vec(),
            super::mime::content_type_for(file_path),
            200,
        ),
        None => match UI_DIST.get_file("index.html") {
            Some(f) => (f.contents().to_vec(), "text/html; charset=utf-8", 200),
            None => (b"UI not built".to_vec(), "text/plain", 500),
        },
    }
}

/// The axum fallback route: anything the API router didn't claim is a UI
/// path.
pub(crate) async fn static_handler(uri: Uri) -> Response {
    let (body, content_type, status) = static_response(uri.path());
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        [(header::CONTENT_TYPE, content_type)],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_path_falls_back_to_the_spa_entry_point() {
        let (index, _, _) = static_response("/");
        let (deep_link, content_type, status) = static_response("/some/client/route");
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/html; charset=utf-8");
        assert_eq!(deep_link, index);
    }

    #[test]
    fn a_real_asset_is_served_under_its_own_content_type() {
        let (body, content_type, status) = static_response("/index.html");
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/html; charset=utf-8");
        assert!(!body.is_empty());
    }
}
