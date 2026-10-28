//! fghjd's control API over its unix socket, for the commands that drive a
//! workspace. hyper rather than the hand-rolled request in `main.rs`, because
//! `logs -f` needs a streamed, chunked body.

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::Value;

async fn send(method: Method, path: &str, body: Option<&Value>) -> Result<Response<Incoming>> {
    let socket = fghj::daemon::socket_path();
    let stream = tokio::net::UnixStream::connect(&socket)
        .await
        .with_context(|| {
            format!(
                "couldn't reach fghjd at {} — is it running? (`fghj doctor`)",
                socket.display()
            )
        })?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .context("couldn't talk to fghjd")?;
    tokio::spawn(conn);
    let body = body.map(Value::to_string).unwrap_or_default();
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, "fghjd")
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))?;
    Ok(sender.send_request(request).await?)
}

/// A non-2xx reply is an error carrying fghjd's own `error` message.
pub async fn json(method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
    let response = send(method, path, body).await?;
    let status = response.status();
    let bytes = response.into_body().collect().await?.to_bytes();
    let value: Value = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "fghjd answered {path} with something that isn't JSON ({status}); \
             if it's older than this CLI, restart it on the current build"
        )
    })?;
    if !status.is_success() {
        match value["error"].as_str() {
            Some(error) => bail!("{error}"),
            None => bail!("fghjd answered {path} with {status}: {value}"),
        }
    }
    Ok(value)
}

pub async fn get(path: &str) -> Result<Value> {
    json(Method::GET, path, None).await
}

pub async fn post(path: &str, body: &Value) -> Result<Value> {
    json(Method::POST, path, Some(body)).await
}

/// The body as it arrives, for server-sent events.
pub async fn stream(path: &str) -> Result<Incoming> {
    let response = send(Method::GET, path, None).await?;
    let status = response.status();
    if !status.is_success() {
        let bytes = response.into_body().collect().await?.to_bytes();
        let value: Value = serde_json::from_slice(&bytes).unwrap_or_default();
        match value["error"].as_str() {
            Some(error) => bail!("{error}"),
            None => bail!("fghjd answered {path} with {status}"),
        }
    }
    Ok(response.into_body())
}
