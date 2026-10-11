use std::sync::Arc;

use axum::Json;
use axum::extract::Path as AxumPath;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::web::api::error::bad_request;
use crate::web::api::extract::{ActorExtractor, WorkspaceExtractor};
use crate::{docker, orchestrator};

#[derive(Deserialize)]
pub(crate) struct ExecStart {
    cmd: Vec<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    tty: bool,
    #[serde(default)]
    cols: u16,
    #[serde(default)]
    rows: u16,
}

/// Text messages are control (JSON); binary messages are stdin.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ExecControl {
    Resize { cols: u16, rows: u16 },
}

/// Resolves the node before the upgrade, so a bad id is a clean 404.
pub(crate) async fn get_node_exec_ws(
    AxumPath(node_id): AxumPath<String>,
    WorkspaceExtractor(state): WorkspaceExtractor,
    ActorExtractor(actor): ActorExtractor,
    ws: WebSocketUpgrade,
) -> Response {
    let run_state = match actor.current().env.clone() {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "no environment yet" })),
            )
                .into_response();
        }
    };
    let container_name = match orchestrator::container_name_for(&run_state, &node_id) {
        Ok(name) => name.to_string(),
        Err(e) => return bad_request(e),
    };

    let docker = state.docker.clone();
    ws.on_upgrade(move |socket| handle_exec_socket(socket, docker, container_name))
}

/// Any failure ends the task: once the socket is broken there's no one to tell.
pub(crate) async fn handle_exec_socket(
    mut socket: WebSocket,
    docker: Arc<bollard::Docker>,
    container_name: String,
) {
    let start = match socket.recv().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<ExecStart>(&text) {
            Ok(start) => start,
            Err(e) => {
                let _ = socket
                    .send(exec_error_message(&format!(
                        "invalid exec start message: {e}"
                    )))
                    .await;
                return;
            }
        },
        _ => return,
    };

    let mut session = match docker::exec_start(
        &docker,
        &container_name,
        &start.cmd,
        start.user.as_deref(),
        start.working_dir.as_deref(),
        start.tty,
    )
    .await
    {
        Ok(session) => session,
        Err(e) => {
            let _ = socket.send(exec_error_message(&e.to_string())).await;
            return;
        }
    };

    if start.tty {
        let _ =
            docker::exec_resize(&docker, &session.id, start.cols.max(1), start.rows.max(1)).await;
    }

    loop {
        tokio::select! {
            item = session.output.next() => {
                match item {
                    Some(Ok(chunk)) => {
                        if socket.send(Message::Binary(chunk.into_bytes())).await.is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Binary(bytes))) if bytes.is_empty() => {
                        // Close the exec's stdin but keep the socket, so a command reading to EOF can
                        // finish and send its output.
                        let _ = session.input.shutdown().await;
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if session.input.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(ExecControl::Resize { cols, rows }) = serde_json::from_str(&text) {
                            let _ = docker::exec_resize(&docker, &session.id, cols, rows).await;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        }
    }

    let code = docker::exec_exit_code(&docker, &session.id)
        .await
        .unwrap_or(-1);
    let _ = socket
        .send(Message::Text(
            serde_json::json!({ "type": "exit", "code": code })
                .to_string()
                .into(),
        ))
        .await;
}

pub(crate) fn exec_error_message(message: &str) -> Message {
    Message::Text(
        serde_json::json!({ "type": "error", "message": message })
            .to_string()
            .into(),
    )
}
