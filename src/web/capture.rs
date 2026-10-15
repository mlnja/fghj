//! The link that brings a sidecar's captured traffic into fghjd.
//!
//! The sidecar runs the same proxy and [`TrafficStore`] code as fghjd, but in
//! its own container. A host Unix socket doesn't cross into one, so the
//! sidecar listens on [`CAPTURE_PORT`], which Docker publishes on the host's
//! 127.0.0.1, and fghjd dials in. One long `POST /capture` carries both
//! directions: fghjd streams the recording set up as JSON lines, and the
//! sidecar streams back every exchange as it changes, one [`Shipped`] per
//! line.
//!
//! The sidecar records only while that request is open. When fghjd stops
//! recording, restarts or crashes, the connection ends and the sidecar stops
//! and clears, so nothing is recorded that nobody will see.
//!
//! The port is reachable by every container on the network and by anything
//! on the host's loopback, so the request must carry the token fghjd wrote
//! into the sidecar's read-only routes folder.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures_util::stream;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, oneshot, watch};

use crate::web::traffic::{BodyMode, Changed, Shipped, TrafficStore};

/// Inside the sidecar. Docker picks the host side.
pub const CAPTURE_PORT: u16 = 7480;
/// In the sidecar's routes folder.
pub const TOKEN_FILE: &str = "capture-token";
/// The sidecar has one workspace, so its store keys every log under this.
pub const SIDECAR_WORKSPACE: &str = "";
/// Which run of the sidecar shipped an exchange: its ids restart with it.
const ORIGIN_HEADER: &str = "x-fghj-capture-origin";

pub type Modes = HashMap<String, BodyMode>;

/// A fresh random token, hex.
pub fn new_token() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("failed to read /dev/urandom")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The sidecar's side. The token is read on every request, so it can be
/// replaced without restarting the sidecar.
pub async fn serve(listener: TcpListener, store: Arc<TrafficStore>, token_path: PathBuf) {
    let origin = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.as_nanos() as u64);
    // Only the newest session's end stops recording: a reconnect may arrive
    // before the old connection is noticed as gone.
    let session = Arc::new(AtomicU64::new(0));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let (store, token_path, session) = (store.clone(), token_path.clone(), session.clone());
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                let (store, token_path, session) =
                    (store.clone(), token_path.clone(), session.clone());
                async move { Ok::<_, Infallible>(handle(req, store, &token_path, session, origin)) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

type Reply = Response<BoxBody<Bytes, Infallible>>;

fn status(code: StatusCode) -> Reply {
    let mut resp = Response::new(Empty::new().boxed());
    *resp.status_mut() = code;
    resp
}

fn authorized(req: &Request<Incoming>, token_path: &std::path::Path) -> bool {
    let Ok(expected) = std::fs::read_to_string(token_path) else {
        return false;
    };
    let expected = expected.trim();
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    !expected.is_empty()
        && expected.len() == given.len()
        && expected
            .bytes()
            .zip(given.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn handle(
    req: Request<Incoming>,
    store: Arc<TrafficStore>,
    token_path: &std::path::Path,
    session: Arc<AtomicU64>,
    origin: u64,
) -> Reply {
    if req.uri().path() != "/capture" {
        return status(StatusCode::NOT_FOUND);
    }
    if req.method() != Method::POST {
        return status(StatusCode::METHOD_NOT_ALLOWED);
    }
    if !authorized(&req, token_path) {
        return status(StatusCode::UNAUTHORIZED);
    }

    let id = session.fetch_add(1, Ordering::SeqCst) + 1;
    let (ended, ended_rx) = oneshot::channel::<()>();
    {
        let store = store.clone();
        let body = req.into_body();
        tokio::spawn(async move {
            read_modes(body, &store).await;
            if session.load(Ordering::SeqCst) == id {
                store.apply_modes(SIDECAR_WORKSPACE, &Modes::new());
            }
            let _ = ended.send(());
        });
    }

    // Subscribed before the snapshot, so nothing falls between the two.
    let changes = store.subscribe_all();
    let queue: VecDeque<Changed> = store.snapshot_all().into();
    let lines = stream::unfold(
        (changes, ended_rx, queue, store),
        |(mut changes, mut ended, mut queue, store)| async move {
            loop {
                if let Some(c) = queue.pop_front() {
                    let line = Frame::data(Bytes::from(c.ex.ship(&c.node)));
                    return Some((Ok(line), (changes, ended, queue, store)));
                }
                tokio::select! {
                    _ = &mut ended => return None,
                    changed = changes.recv() => match changed {
                        Ok(c) => queue.push_back(c),
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            queue = store.snapshot_all().into();
                        }
                        Err(broadcast::error::RecvError::Closed) => return None,
                    },
                }
            }
        },
    );
    let mut resp = Response::new(BodyExt::boxed(StreamBody::new(lines)));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    resp.headers_mut()
        .insert(ORIGIN_HEADER, HeaderValue::from(origin));
    resp
}

/// Applies each recording set fghjd sends until its request body ends.
async fn read_modes(mut body: Incoming, store: &TrafficStore) {
    let mut lines = Lines::default();
    while let Some(Ok(frame)) = body.frame().await {
        let Some(data) = frame.data_ref() else {
            continue;
        };
        for line in lines.push(data) {
            if let Ok(modes) = serde_json::from_slice::<Modes>(&line) {
                store.apply_modes(SIDECAR_WORKSPACE, &modes);
            }
        }
    }
}

/// Splits a byte stream into newline-terminated lines.
#[derive(Default)]
struct Lines {
    buf: Vec<u8>,
}

impl Lines {
    fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let mut lines = Vec::new();
        while let Some(at) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=at).collect();
            line.pop();
            lines.push(line);
        }
        lines
    }
}

/// fghjd's side: one session with a sidecar. Sends `modes` now and whenever
/// it changes, and hands every shipped exchange to `ingest` with the
/// sidecar's origin. Returns when the connection ends, an error saying why.
pub async fn pull(
    addr: SocketAddr,
    token: &str,
    modes: watch::Receiver<Modes>,
    mut ingest: impl FnMut(u64, Shipped),
) -> Result<()> {
    let tcp = TcpStream::connect(addr)
        .await
        .with_context(|| format!("failed to connect to the sidecar's capture port {addr}"))?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
        .await
        .context("capture handshake failed")?;

    let sets = stream::unfold((modes, true), |(mut modes, first)| async move {
        if !first && modes.changed().await.is_err() {
            return None;
        }
        let mut line = serde_json::to_vec(&*modes.borrow_and_update()).ok()?;
        line.push(b'\n');
        Some((
            Ok::<_, Infallible>(Frame::data(Bytes::from(line))),
            (modes, false),
        ))
    });
    let req = Request::post("/capture")
        .header(header::HOST, "fghj-sidecar")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .body(StreamBody::new(sets))
        .context("failed to build the capture request")?;

    // Its own task, so a refusal is read before the connection's close.
    let conn = AbortOnDrop(tokio::spawn(conn).abort_handle());
    let session = async {
        let resp = sender
            .send_request(req)
            .await
            .context("capture request failed")?;
        if resp.status() != StatusCode::OK {
            bail!("the sidecar refused the capture request: {}", resp.status());
        }
        let origin = resp
            .headers()
            .get(ORIGIN_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .context("the sidecar didn't say which run it is")?;
        let mut body = resp.into_body();
        let mut lines = Lines::default();
        while let Some(frame) = body.frame().await {
            let frame = frame.context("capture stream broke")?;
            let Some(data) = frame.data_ref() else {
                continue;
            };
            for line in lines.push(data) {
                match serde_json::from_slice::<Shipped>(&line) {
                    Ok(shipped) => ingest(origin, shipped),
                    Err(e) => bail!("the sidecar shipped an exchange fghjd can't read: {e}"),
                }
            }
        }
        bail!("the sidecar ended the capture stream")
    };
    let result = session.await;
    drop(conn);
    result
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::traffic::{Side, TrafficLog};
    use http_body_util::Full;
    use std::time::Duration;

    struct Sidecar {
        addr: SocketAddr,
        store: Arc<TrafficStore>,
        token: String,
        _dir: tempfile::TempDir,
    }

    async fn sidecar() -> Sidecar {
        let dir = tempfile::tempdir().unwrap();
        let token = new_token().unwrap();
        let token_path = dir.path().join(TOKEN_FILE);
        std::fs::write(&token_path, &token).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let store = Arc::new(TrafficStore::default());
        tokio::spawn(serve(listener, store.clone(), token_path));
        Sidecar {
            addr,
            store,
            token,
            _dir: dir,
        }
    }

    /// One exchange through `log`, the way the proxy records it.
    async fn record(log: &Arc<TrafficLog>, path: &str) {
        let req = Request::post(path)
            .header("host", "api.fghj.internal")
            .header("content-type", "application/json")
            .body(())
            .unwrap();
        let rec = log.begin(&req, "172.18.0.5".into(), "https").unwrap();
        rec.tap(Side::Request, Full::new(Bytes::from_static(b"{\"a\":1}")))
            .collect()
            .await
            .unwrap();
        let resp = Response::builder()
            .status(201)
            .header("content-type", "application/json")
            .body(())
            .unwrap();
        rec.response(&resp, false);
        rec.tap(
            Side::Response,
            Full::new(Bytes::from_static(b"{\"ok\":true}")),
        )
        .collect()
        .await
        .unwrap();
    }

    async fn eventually(mut check: impl FnMut() -> bool) {
        for _ in 0..200 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition never held");
    }

    fn fghjd() -> (Arc<TrafficStore>, Arc<TrafficLog>) {
        let store = Arc::new(TrafficStore::default());
        let log = store.log("ws", "api");
        log.set_mode(Some(BodyMode::Text));
        (store, log)
    }

    fn spawn_pull(
        side: &Sidecar,
        modes: watch::Receiver<Modes>,
        log: Arc<TrafficLog>,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let (addr, token) = (side.addr, side.token.clone());
        tokio::spawn(async move {
            pull(addr, &token, modes, |origin, shipped| {
                log.ingest(origin, shipped)
            })
            .await
        })
    }

    #[tokio::test]
    async fn the_sidecar_records_only_while_fghjd_is_connected() {
        let side = sidecar().await;
        let remote = side.store.log(SIDECAR_WORKSPACE, "api");
        assert_eq!(remote.mode(), None);

        let (_store, local) = fghjd();
        let (modes, modes_rx) = watch::channel(Modes::from([("api".into(), BodyMode::Text)]));
        let task = spawn_pull(&side, modes_rx, local.clone());
        eventually(|| remote.mode() == Some(BodyMode::Text)).await;

        record(&remote, "/things").await;
        eventually(|| local.summaries().iter().any(|s| s.state == "done")).await;
        let summaries = local.summaries();
        assert_eq!(summaries.len(), 1, "updates fold into one exchange");
        let ex = local.get(summaries[0].id).unwrap();
        let detail = serde_json::to_value(ex.detail()).unwrap();
        assert_eq!(detail["status"], 201);
        assert_eq!(detail["source"], "172.18.0.5");
        assert_eq!(detail["request"]["body"]["text"], "{\"a\":1}");
        assert_eq!(detail["response"]["body"]["text"], "{\"ok\":true}");

        // A changed set goes up the open request.
        modes
            .send(Modes::from([("api".into(), BodyMode::All)]))
            .unwrap();
        eventually(|| remote.mode() == Some(BodyMode::All)).await;

        // Hanging up stops and clears the sidecar.
        task.abort();
        eventually(|| remote.mode().is_none() && remote.summaries().is_empty()).await;
    }

    #[tokio::test]
    async fn a_node_left_out_of_the_set_stops_and_clears() {
        let side = sidecar().await;
        let remote = side.store.log(SIDECAR_WORKSPACE, "api");
        let (_store, local) = fghjd();
        let (modes, modes_rx) = watch::channel(Modes::from([("api".into(), BodyMode::Text)]));
        let _task = spawn_pull(&side, modes_rx, local.clone());
        eventually(|| remote.mode().is_some()).await;
        record(&remote, "/one").await;
        eventually(|| local.summaries().len() == 1).await;

        modes.send(Modes::new()).unwrap();
        eventually(|| remote.mode().is_none() && remote.summaries().is_empty()).await;
        assert_eq!(local.summaries().len(), 1, "fghjd keeps what it has");
    }

    #[tokio::test]
    async fn a_wrong_token_is_refused() {
        let side = sidecar().await;
        let (_store, local) = fghjd();
        let (_modes, modes_rx) = watch::channel(Modes::from([("api".into(), BodyMode::Text)]));
        let err = pull(side.addr, "nope", modes_rx, |origin, shipped| {
            local.ingest(origin, shipped)
        })
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("401"), "{err:#}");
        assert_eq!(side.store.log(SIDECAR_WORKSPACE, "api").mode(), None);
    }

    #[tokio::test]
    async fn a_shipped_exchange_from_before_a_clear_stays_cleared() {
        let side = sidecar().await;
        let remote = side.store.log(SIDECAR_WORKSPACE, "api");
        remote.set_mode(Some(BodyMode::Text));
        record(&remote, "/old").await;
        let old = remote.summaries()[0].id;
        let (_store, local) = fghjd();

        let shipped: Shipped =
            serde_json::from_slice(&remote.get(old).unwrap().ship("api")).unwrap();
        local.ingest(7, shipped);
        assert_eq!(local.summaries().len(), 1);

        tokio::time::sleep(Duration::from_millis(5)).await;
        local.clear();
        let shipped: Shipped =
            serde_json::from_slice(&remote.get(old).unwrap().ship("api")).unwrap();
        local.ingest(7, shipped);
        assert!(local.summaries().is_empty());
    }

    #[tokio::test]
    async fn the_same_line_received_twice_is_one_exchange() {
        let remote = TrafficStore::default().log(SIDECAR_WORKSPACE, "api");
        remote.set_mode(Some(BodyMode::Text));
        record(&remote, "/once").await;
        let line = remote.get(remote.summaries()[0].id).unwrap().ship("api");
        let (_store, local) = fghjd();

        for _ in 0..2 {
            local.ingest(7, serde_json::from_slice(&line).unwrap());
        }
        assert_eq!(local.summaries().len(), 1);

        // The same id from another run of the sidecar is a different exchange.
        local.ingest(8, serde_json::from_slice(&line).unwrap());
        assert_eq!(local.summaries().len(), 2);
    }
}
