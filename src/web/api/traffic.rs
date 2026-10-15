//! Recorded proxy traffic: switch recording per node, follow it live, and
//! open single exchanges. See `web::traffic`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use crate::daemon::registry::WorkspaceRegistry;
use crate::web::traffic::{BodyMode, Side, TrafficLog, Update};

pub(crate) fn routes() -> Router<Arc<WorkspaceRegistry>> {
    Router::new()
        .route("/traffic/recording", get(get_traffic_recording))
        .route(
            "/nodes/{node_id}/traffic/recording",
            post(post_node_traffic_recording),
        )
        .route(
            "/nodes/{node_id}/traffic/clear",
            post(post_node_traffic_clear),
        )
        .route(
            "/nodes/{node_id}/traffic/stream",
            get(get_node_traffic_stream),
        )
        .route(
            "/nodes/{node_id}/traffic/{id}",
            get(get_node_traffic_exchange),
        )
        .route(
            "/nodes/{node_id}/traffic/{id}/body/{side}",
            get(get_node_traffic_body),
        )
}

#[derive(Deserialize)]
struct WorkspaceQuery {
    workspace: Option<String>,
}

fn workspace_id(registry: &WorkspaceRegistry, q: WorkspaceQuery) -> Option<String> {
    q.workspace.filter(|id| registry.get(id).is_some())
}

fn node_log(
    registry: &WorkspaceRegistry,
    q: WorkspaceQuery,
    node_id: &str,
) -> Option<Arc<TrafficLog>> {
    let ws = workspace_id(registry, q)?;
    Some(registry.traffic().log(&ws, node_id))
}

fn unknown_workspace() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": "unknown or missing ?workspace=<id>; POST /workspaces first" })),
    )
        .into_response()
}

fn not_found(what: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": what })),
    )
        .into_response()
}

/// Which nodes are recording, for the dot on the graph.
async fn get_traffic_recording(
    State(registry): State<Arc<WorkspaceRegistry>>,
    Query(q): Query<WorkspaceQuery>,
) -> Response {
    match workspace_id(&registry, q) {
        Some(ws) => {
            let nodes: HashMap<String, BodyMode> = registry.traffic().recording(&ws);
            Json(serde_json::json!({ "nodes": nodes })).into_response()
        }
        None => unknown_workspace(),
    }
}

#[derive(Deserialize)]
struct RecordingBody {
    /// `null` stops recording.
    mode: Option<BodyMode>,
}

async fn post_node_traffic_recording(
    State(registry): State<Arc<WorkspaceRegistry>>,
    AxumPath(node_id): AxumPath<String>,
    Query(q): Query<WorkspaceQuery>,
    Json(body): Json<RecordingBody>,
) -> Response {
    match node_log(&registry, q, &node_id) {
        Some(log) => {
            log.set_mode(body.mode);
            Json(serde_json::json!({ "mode": body.mode })).into_response()
        }
        None => unknown_workspace(),
    }
}

async fn post_node_traffic_clear(
    State(registry): State<Arc<WorkspaceRegistry>>,
    AxumPath(node_id): AxumPath<String>,
    Query(q): Query<WorkspaceQuery>,
) -> Response {
    match node_log(&registry, q, &node_id) {
        Some(log) => {
            log.clear();
            StatusCode::NO_CONTENT.into_response()
        }
        None => unknown_workspace(),
    }
}

/// A snapshot first, then every change. A subscriber that falls behind gets
/// a fresh snapshot rather than a gap.
async fn get_node_traffic_stream(
    State(registry): State<Arc<WorkspaceRegistry>>,
    AxumPath(node_id): AxumPath<String>,
    Query(q): Query<WorkspaceQuery>,
) -> Response {
    let Some(log) = node_log(&registry, q, &node_id) else {
        return unknown_workspace();
    };
    let rx = log.subscribe();
    let first = snapshot(&log);
    let rest = futures_util::stream::unfold((log, rx), |(log, mut rx)| async move {
        let event = match rx.recv().await {
            Ok(update) => update_event(update),
            Err(RecvError::Lagged(_)) => snapshot(&log),
            Err(RecvError::Closed) => return None,
        };
        Some((event, (log, rx)))
    });
    let stream = futures_util::stream::once(async move { first })
        .chain(rest)
        .map(Ok::<Event, Infallible>);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn snapshot(log: &TrafficLog) -> Event {
    json_event(serde_json::json!({
        "type": "snapshot",
        "mode": log.mode(),
        "exchanges": log.summaries(),
    }))
}

fn update_event(update: Update) -> Event {
    json_event(match update {
        Update::Upsert(summary) => serde_json::json!({ "type": "upsert", "exchange": summary }),
        Update::Mode(mode) => serde_json::json!({ "type": "mode", "mode": mode }),
        Update::Cleared => serde_json::json!({ "type": "cleared" }),
    })
}

fn json_event(value: serde_json::Value) -> Event {
    Event::default().data(value.to_string())
}

async fn get_node_traffic_exchange(
    State(registry): State<Arc<WorkspaceRegistry>>,
    AxumPath((node_id, id)): AxumPath<(String, u64)>,
    Query(q): Query<WorkspaceQuery>,
) -> Response {
    let Some(log) = node_log(&registry, q, &node_id) else {
        return unknown_workspace();
    };
    match log.get(id) {
        Some(ex) => Json(ex.detail()).into_response(),
        None => not_found("no such exchange; it may have been evicted or cleared"),
    }
}

/// The decoded body as the app sent it, for image previews and downloads.
/// It's arbitrary content served from fghj's own origin, so it is sandboxed
/// and, unless it's an image, downloaded rather than rendered.
async fn get_node_traffic_body(
    State(registry): State<Arc<WorkspaceRegistry>>,
    AxumPath((node_id, id, side)): AxumPath<(String, u64, Side)>,
    Query(q): Query<WorkspaceQuery>,
) -> Response {
    let Some(log) = node_log(&registry, q, &node_id) else {
        return unknown_workspace();
    };
    let Some((bytes, content_type)) = log.get(id).and_then(|ex| ex.body(side)) else {
        return not_found("no body was kept for this exchange");
    };
    let content_type = content_type
        .and_then(|ct| HeaderValue::from_str(&ct).ok())
        .unwrap_or(HeaderValue::from_static("application/octet-stream"));
    let is_image = content_type
        .to_str()
        .is_ok_and(|ct| ct.trim().to_ascii_lowercase().starts_with("image/"));
    let mut resp = bytes.into_response();
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if !is_image {
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::registry::tests::test_docker;
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::Request;

    struct Api {
        addr: std::net::SocketAddr,
        registry: Arc<WorkspaceRegistry>,
        ws: String,
        _tmp: tempfile::TempDir,
    }

    async fn api() -> Api {
        let tmp = tempfile::tempdir().unwrap();
        let registry = Arc::new(
            WorkspaceRegistry::load_from(tmp.path().join("workspaces.json"), test_docker()).await,
        );
        let (ws, _) = registry
            .resolve(None, Some(tmp.path().join("shop")), None)
            .await
            .unwrap();
        let app = routes().with_state(registry.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        Api {
            addr,
            registry,
            ws,
            _tmp: tmp,
        }
    }

    impl Api {
        /// The response head and its body; for an SSE stream only the first
        /// frame, since the stream never ends.
        async fn call(
            &self,
            method: &str,
            path: &str,
            body: &'static str,
        ) -> (hyper::Response<()>, String) {
            let tcp = tokio::net::TcpStream::connect(self.addr).await.unwrap();
            let (mut sender, conn) =
                hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tcp))
                    .await
                    .unwrap();
            tokio::spawn(conn);
            let sep = if path.contains('?') { '&' } else { '?' };
            let req = Request::builder()
                .method(method)
                .uri(format!("{path}{sep}workspace={}", self.ws))
                .header("host", "fghj.internal")
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from_static(body.as_bytes())))
                .unwrap();
            let resp = sender.send_request(req).await.unwrap();
            let (head, mut body) = resp.into_parts();
            let text = if head
                .headers
                .get("content-type")
                .is_some_and(|ct| ct == "text/event-stream")
            {
                let frame = body.frame().await.unwrap().unwrap();
                String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap()
            } else {
                String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap()
            };
            (hyper::Response::from_parts(head, ()), text)
        }
    }

    #[tokio::test]
    async fn recording_is_switched_listed_followed_and_opened_over_the_api() {
        let api = api().await;
        let (resp, _) = api
            .call("POST", "/nodes/api/traffic/recording", r#"{"mode":"text"}"#)
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let (_, listed) = api.call("GET", "/traffic/recording", "").await;
        assert_eq!(listed, r#"{"nodes":{"api":"text"}}"#);

        let log = api.registry.traffic().log(&api.ws, "api");
        let req = Request::builder().uri("/hello").body(()).unwrap();
        let rec = log.begin(&req, "host".into(), "https").unwrap();
        let resp = hyper::Response::builder()
            .header("content-type", "image/png")
            .body(())
            .unwrap();
        rec.response(&resp, false);
        rec.tap(Side::Response, Full::new(Bytes::from_static(b"\x89PNG")))
            .collect()
            .await
            .unwrap();
        let id = log.summaries()[0].id;

        let (_, stream) = api.call("GET", "/nodes/api/traffic/stream", "").await;
        let snapshot: serde_json::Value =
            serde_json::from_str(stream.trim().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(snapshot["type"], "snapshot");
        assert_eq!(snapshot["mode"], "text");
        assert_eq!(snapshot["exchanges"][0]["url"], "/hello");

        let (resp, detail) = api
            .call("GET", &format!("/nodes/api/traffic/{id}"), "")
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        assert_eq!(detail["response"]["body"]["omitted"], "binary");

        // Text mode dropped the PNG, so there's nothing to serve.
        let (resp, _) = api
            .call("GET", &format!("/nodes/api/traffic/{id}/body/response"), "")
            .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let (resp, _) = api.call("POST", "/nodes/api/traffic/clear", "").await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(log.summaries().is_empty());

        api.call("POST", "/nodes/api/traffic/recording", r#"{"mode":null}"#)
            .await;
        let (_, listed) = api.call("GET", "/traffic/recording", "").await;
        assert_eq!(listed, r#"{"nodes":{}}"#);
    }

    #[tokio::test]
    async fn a_kept_body_is_served_sandboxed() {
        let api = api().await;
        let log = api.registry.traffic().log(&api.ws, "api");
        log.set_mode(Some(BodyMode::All));
        let req = Request::builder().uri("/page").body(()).unwrap();
        let rec = log.begin(&req, "host".into(), "https").unwrap();
        let resp = hyper::Response::builder()
            .header("content-type", "text/html")
            .body(())
            .unwrap();
        rec.response(&resp, false);
        rec.tap(
            Side::Response,
            Full::new(Bytes::from_static(b"<script>x</script>")),
        )
        .collect()
        .await
        .unwrap();
        let id = log.summaries()[0].id;

        let (resp, body) = api
            .call("GET", &format!("/nodes/api/traffic/{id}/body/response"), "")
            .await;
        assert_eq!(body, "<script>x</script>");
        let h = resp.headers();
        assert_eq!(h["content-type"], "text/html");
        assert_eq!(h["content-security-policy"], "sandbox; default-src 'none'");
        assert_eq!(h["content-disposition"], "attachment");
    }

    #[tokio::test]
    async fn an_unknown_workspace_is_rejected() {
        let mut api = api().await;
        api.ws = "nope".to_string();
        let (resp, _) = api.call("GET", "/traffic/recording", "").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
