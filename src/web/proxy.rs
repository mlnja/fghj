//! The TLS reverse proxy on 80/443: routes by SNI through a `RouteResolver`.
//! Runs on the host and inside each sidecar.
//!
//! Both legs speak HTTP/1.1 rather than splicing bytes, so that every request
//! on a kept-alive connection gets the `X-Forwarded-*` headers apps use to
//! build their own URLs. Without them a registry or phpMyAdmin behind the TLS
//! listener sees plain HTTP and links back to `http://`.

use std::convert::Infallible;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::upgrade::OnUpgrade;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::dns;
use crate::supervisor;
use crate::web::ca;
use crate::web::traffic::{Recording, Side, TrafficLog};

#[derive(Clone)]
pub struct Backend {
    pub host: String,
    pub port: u16,
    /// The node's traffic log, checked on every request so recording can be
    /// switched on under an already-open connection.
    pub traffic: Option<Arc<TrafficLog>>,
}

pub trait RouteResolver: Send + Sync {
    fn resolve(&self, host: &str) -> Option<Backend>;
}

/// Fixed, not ephemeral: a bare `https://fghj.internal` only reaches 443.
pub const HTTP_PORT: u16 = 80;
pub const HTTPS_PORT: u16 = 443;

/// A dedicated loopback alias rather than 127.0.0.1, so a developer's own
/// :80/:443 keep working; inside 127/8 so it can't collide with a real network.
/// Aliased onto loopback by `platform::Loopback::add_alias` where needed.
pub const PROXY_IP: Ipv4Addr = Ipv4Addr::new(127, 222, 0, 1);

pub async fn bind_http() -> Result<TcpListener> {
    TcpListener::bind((PROXY_IP, HTTP_PORT))
        .await
        .with_context(|| format!("failed to bind fghj's HTTP listener on {PROXY_IP}:{HTTP_PORT} — is something else already using it?"))
}

pub async fn bind_https() -> Result<TcpListener> {
    TcpListener::bind((PROXY_IP, HTTPS_PORT))
        .await
        .with_context(|| format!("failed to bind fghj's HTTPS listener on {PROXY_IP}:{HTTPS_PORT} — is something else already using it?"))
}

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");

type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// What to do with one request: answer it here, or forward it.
enum Route {
    Forward(Backend),
    Respond(Response<ProxyBody>),
}

/// Redirects in-zone and reserved-TLD names to HTTPS. A routed non-reserved
/// `#AdditionalHost` never gets a cert, so it is relayed over plain HTTP instead.
pub async fn serve_http_redirect(listener: TcpListener, routes: Arc<dyn RouteResolver>) {
    println!(
        "fghjd: HTTP listener on 127.0.0.1:{HTTP_PORT}, redirecting everything to https:// except routed non-reserved additional hosts"
    );
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("fghjd: HTTP accept error: {e}");
                continue;
            }
        };
        let routes = routes.clone();
        supervisor::supervise("http connection", async move {
            let route = move |req: &Request<Incoming>| route_plain_http(req, routes.as_ref());
            if let Err(e) = serve_connection(stream, peer.ip(), "http", route).await {
                eprintln!("fghjd: HTTP handler error: {e:#}");
            }
        });
    }
}

fn route_plain_http(req: &Request<Incoming>, routes: &dyn RouteResolver) -> Route {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let plain_backend = (!dns::in_zone(host) && !dns::is_reserved_alias(host))
        .then(|| routes.resolve(host))
        .flatten();
    match plain_backend {
        Some(backend) => Route::Forward(backend),
        None => {
            let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
            Route::Respond(redirect_response(host, path))
        }
    }
}

fn redirect_response(host: &str, path: &str) -> Response<ProxyBody> {
    let mut resp = Response::new(empty());
    *resp.status_mut() = StatusCode::MOVED_PERMANENTLY;
    // Both halves came out of an already-parsed request, so this can't fail.
    if let Ok(location) = HeaderValue::from_str(&format!("https://{host}{path}")) {
        resp.headers_mut().insert(header::LOCATION, location);
    }
    resp
}

fn not_found_response() -> Response<ProxyBody> {
    let body = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>fghj</title>
<style>
  body { background: #0b0b0d; color: #e6e6e6; font-family: ui-monospace, "SF Mono", Menlo, monospace;
         display: flex; align-items: center; justify-content: center; height: 100vh; margin: 0; }
  .card { text-align: center; }
  h1 { font-size: 4rem; margin: 0; color: #ff7a1a; }
  p { color: #999; }
</style></head>
<body><div class="card"><h1>404</h1><p>fghj doesn't know this service yet.</p></div></body></html>
"##;
    let mut resp = Response::new(full(body));
    *resp.status_mut() = StatusCode::NOT_FOUND;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

fn bad_gateway_response(backend: &Backend) -> Response<ProxyBody> {
    let mut resp = Response::new(full(format!(
        "fghj couldn't reach this service at {}:{}\n",
        backend.host, backend.port
    )));
    *resp.status_mut() = StatusCode::BAD_GATEWAY;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

fn full(body: impl Into<Bytes>) -> ProxyBody {
    Full::new(body.into())
        .map_err(|never| match never {})
        .boxed()
}

fn empty() -> ProxyBody {
    Empty::new().map_err(|never| match never {}).boxed()
}

/// The zone apex goes to the control API, other names to their route, anything
/// else to a styled 404.
pub async fn serve_https(
    listener: TcpListener,
    cert_resolver: Arc<ca::DynamicCertResolver>,
    control_port: u16,
    provider: Arc<CryptoProvider>,
    routes: Arc<dyn RouteResolver>,
) {
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring/aws-lc-rs providers always support the default protocol versions")
        .with_no_client_auth()
        .with_cert_resolver(cert_resolver);
    let acceptor = TlsAcceptor::from(Arc::new(config));

    println!(
        "fghjd: HTTPS listener on 127.0.0.1:{HTTPS_PORT}, routing https://{} to the control API",
        dns::ZONE
    );
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("fghjd: HTTPS accept error: {e}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let routes = routes.clone();
        supervisor::supervise("https connection", async move {
            match acceptor.accept(stream).await {
                Ok(tls_stream) => {
                    handle_https_connection(tls_stream, peer.ip(), control_port, routes).await
                }
                Err(e) => report_handshake_failure(&e),
            }
        });
    }
}

/// Why a TLS handshake didn't finish, for deciding whether anyone needs to
/// hear about it.
#[derive(Debug, PartialEq, Eq)]
enum HandshakeFailure {
    /// The client hung up first: browsers pre-connect and abandon sockets all
    /// the time, and port probes never send a hello.
    Abandoned,
    /// A name fghj doesn't serve, so there was no certificate to offer.
    NotOurs,
    /// The client refused the certificate: it doesn't trust fghj's CA.
    Untrusted(rustls::AlertDescription),
    Other,
}

/// `tokio-rustls` wraps rustls's own error in the `io::Error`.
fn classify_handshake_failure(e: &io::Error) -> HandshakeFailure {
    use rustls::AlertDescription::{BadCertificate, CertificateUnknown, UnknownCA};
    if matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
    ) {
        return HandshakeFailure::Abandoned;
    }
    match e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
    {
        Some(rustls::Error::AlertReceived(
            alert @ (UnknownCA | BadCertificate | CertificateUnknown),
        )) => HandshakeFailure::Untrusted(*alert),
        Some(rustls::Error::General(why)) if why.contains("no server certificate chain") => {
            HandshakeFailure::NotOurs
        }
        _ => HandshakeFailure::Other,
    }
}

/// Only what someone could act on, and a refused certificate at most once a
/// minute: one browser that doesn't trust the CA refuses every connection.
fn report_handshake_failure(e: &io::Error) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_UNTRUSTED_MS: AtomicU64 = AtomicU64::new(0);
    const EVERY_MS: u64 = 60_000;

    match classify_handshake_failure(e) {
        HandshakeFailure::Abandoned | HandshakeFailure::NotOurs => {}
        HandshakeFailure::Untrusted(alert) => {
            let now = crate::util::time::now_ms();
            let last = LAST_UNTRUSTED_MS.load(Ordering::Relaxed);
            if now.saturating_sub(last) >= EVERY_MS
                && LAST_UNTRUSTED_MS
                    .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                crate::daemon_log::warn(format!(
                    "fghjd: a client refused fghj's certificate ({alert:?}): it doesn't trust \
                     fghj's CA yet. `fghj doctor` checks the system's and the browsers' stores; \
                     restart a browser that was open when the CA was added"
                ));
            }
        }
        HandshakeFailure::Other => {
            crate::daemon_log::warn(format!("fghjd: TLS handshake error: {e}"))
        }
    }
}

async fn handle_https_connection(
    tls_stream: TlsStream<TcpStream>,
    client_ip: IpAddr,
    control_port: u16,
    routes: Arc<dyn RouteResolver>,
) {
    let server_name = {
        let (_, conn) = tls_stream.get_ref();
        conn.server_name().map(|s| s.to_string())
    };
    let Some(name) = server_name else {
        return;
    };

    let backend = if name == dns::ZONE {
        Some(Backend {
            host: "127.0.0.1".to_string(),
            port: control_port,
            traffic: None,
        })
    } else {
        routes.resolve(&name)
    };

    let route = move |_: &Request<Incoming>| match &backend {
        Some(backend) => Route::Forward(backend.clone()),
        None => Route::Respond(not_found_response()),
    };
    if let Err(e) = serve_connection(tls_stream, client_ip, "https", route).await {
        eprintln!("fghjd: proxy error ({name}): {e:#}");
    }
}

/// One client connection. Each request is answered or forwarded as `route`
/// decides, with `X-Forwarded-*` added on the way through.
async fn serve_connection<S, R>(
    io: S,
    client_ip: IpAddr,
    proto: &'static str,
    route: R,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    R: Fn(&Request<Incoming>) -> Route + Send + Sync + 'static,
{
    let route = Arc::new(route);
    // One backend connection per client connection, reused across its
    // requests, the way the old byte splice mapped them one to one.
    let upstream: Arc<Mutex<Option<Upstream>>> = Arc::default();
    let service = hyper::service::service_fn(move |req| {
        let route = route.clone();
        let upstream = upstream.clone();
        async move {
            let resp = match route(&req) {
                Route::Respond(resp) => resp,
                Route::Forward(backend) => {
                    let rec = backend
                        .traffic
                        .as_ref()
                        .and_then(|log| log.begin(&req, traffic_source(client_ip), proto));
                    forward(req, &backend, client_ip, proto, &upstream, rec.as_ref())
                        .await
                        .unwrap_or_else(|e| {
                            eprintln!(
                                "fghjd: backend proxy error ({}:{}): {e:#}",
                                backend.host, backend.port
                            );
                            if let Some(rec) = &rec {
                                rec.fail(&e);
                            }
                            bad_gateway_response(&backend)
                        })
                }
            };
            Ok::<_, Infallible>(resp)
        }
    });

    let result = hyper::server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .timer(TokioTimer::new())
        .serve_connection(TokioIo::new(io), service)
        .with_upgrades()
        .await;
    match result {
        Err(e) if !is_benign_client_error(&e) => Err(e).context("client connection failed"),
        _ => Ok(()),
    }
}

struct Upstream {
    host: String,
    port: u16,
    sender: SendRequest<ProxyBody>,
}

async fn forward(
    mut req: Request<Incoming>,
    backend: &Backend,
    client_ip: IpAddr,
    proto: &'static str,
    upstream: &Mutex<Option<Upstream>>,
    rec: Option<&Recording>,
) -> Result<Response<ProxyBody>> {
    let client_upgrade = req
        .headers()
        .contains_key(header::UPGRADE)
        .then(|| hyper::upgrade::on(&mut req));
    strip_hop_by_hop(req.headers_mut(), client_upgrade.is_some());
    add_forwarded_headers(req.headers_mut(), client_ip, proto);

    let mut slot = upstream.lock().await;
    let reusable = match slot.take() {
        Some(mut u) if u.host == backend.host && u.port == backend.port => {
            u.sender.ready().await.is_ok().then_some(u.sender)
        }
        _ => None,
    };
    let reused = reusable.is_some();
    let mut sender = match reusable {
        Some(sender) => sender,
        None => connect(backend).await?,
    };

    // A reused connection can die under a request that was already written:
    // the backend closed it as idle just as the request went out. Requests
    // that are safe to repeat get one more go on a fresh connection, the way
    // nginx's `proxy_next_upstream error` does.
    let replay = (reused && is_replayable(&req)).then(|| bodyless_copy(&req));

    let req = req.map(|body| match rec {
        Some(rec) => rec.tap(Side::Request, body).boxed(),
        None => body.boxed(),
    });
    let mut resp = match sender.try_send_request(req).await {
        Ok(resp) => resp,
        Err(mut e) => {
            // A message handed back never reached the backend, so any method
            // may go again; otherwise only the replayable copy may.
            let again = e.take_message().filter(|_| reused).or(replay);
            let Some(req) = again else {
                return Err(e.into_error()).context("backend request failed");
            };
            sender = connect(backend).await?;
            sender
                .send_request(req)
                .await
                .context("backend request failed")?
        }
    };

    let upgraded = resp.status() == StatusCode::SWITCHING_PROTOCOLS;
    strip_hop_by_hop(resp.headers_mut(), upgraded);
    if let Some(rec) = rec {
        rec.response(&resp, upgraded);
    }
    if upgraded {
        // The backend connection now belongs to the upgraded stream.
        if let Some(client_upgrade) = client_upgrade {
            let backend_upgrade = hyper::upgrade::on(&mut resp);
            supervisor::supervise(
                "proxied upgrade",
                splice_upgraded(client_upgrade, backend_upgrade, rec.cloned()),
            );
        }
    } else {
        *slot = Some(Upstream {
            host: backend.host.clone(),
            port: backend.port,
            sender,
        });
    }
    Ok(resp.map(|body| match rec {
        Some(rec) if !upgraded => rec.tap(Side::Response, body).boxed(),
        _ => body.boxed(),
    }))
}

/// Who sent a recorded request. Everything on loopback is the developer's
/// own machine: a browser, curl, or a container through Docker's port mapping.
fn traffic_source(client_ip: IpAddr) -> String {
    if client_ip.is_loopback() {
        "host".to_string()
    } else {
        client_ip.to_string()
    }
}

/// Safe to send twice, and nothing of it is lost by having been sent once.
fn is_replayable(req: &Request<Incoming>) -> bool {
    use hyper::Method;
    matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS)
        && hyper::body::Body::is_end_stream(req.body())
}

fn bodyless_copy(req: &Request<Incoming>) -> Request<ProxyBody> {
    let mut copy = Request::new(empty());
    *copy.method_mut() = req.method().clone();
    *copy.uri_mut() = req.uri().clone();
    *copy.version_mut() = req.version();
    *copy.headers_mut() = req.headers().clone();
    // Carries hyper's record of the client's header case.
    *copy.extensions_mut() = req.extensions().clone();
    copy
}

/// Hop-by-hop headers (RFC 9110 §7.6.1) describe one connection, and each leg
/// here is its own, framed by hyper. Passed through, a backend's
/// `Connection: close` would close the browser's connection too. An upgrade
/// is the exception: its `Connection`/`Upgrade` pair is what both ends agree on.
fn strip_hop_by_hop(headers: &mut HeaderMap, upgrade: bool) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .collect();
    for name in named {
        if !(upgrade && name == header::UPGRADE) {
            headers.remove(name);
        }
    }
    for name in [
        HeaderName::from_static("keep-alive"),
        HeaderName::from_static("proxy-connection"),
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
    ] {
        headers.remove(name);
    }
    if upgrade {
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    } else {
        headers.remove(header::CONNECTION);
        headers.remove(header::UPGRADE);
    }
}

async fn connect(backend: &Backend) -> Result<SendRequest<ProxyBody>> {
    let tcp = TcpStream::connect((backend.host.as_str(), backend.port))
        .await
        .with_context(|| {
            format!(
                "failed to connect to backend on {}:{}",
                backend.host, backend.port
            )
        })?;
    let (sender, conn) = hyper::client::conn::http1::Builder::new()
        .preserve_header_case(true)
        .handshake(TokioIo::new(tcp))
        .await
        .context("HTTP handshake with backend failed")?;
    // Its failures surface on the request that was in flight, so they are
    // reported there rather than twice.
    supervisor::supervise("backend connection", async move {
        let _ = conn.with_upgrades().await;
    });
    Ok(sender)
}

/// Present values are kept rather than replaced: fghjd's clients are the
/// developer's own tools and containers, and one of them may be a gateway
/// that already recorded the browser's view of the request.
fn add_forwarded_headers(headers: &mut HeaderMap, client_ip: IpAddr, proto: &'static str) {
    if !headers.contains_key(X_FORWARDED_PROTO) {
        headers.insert(X_FORWARDED_PROTO, HeaderValue::from_static(proto));
    }
    if !headers.contains_key(X_FORWARDED_HOST)
        && let Some(host) = headers.get(header::HOST).cloned()
    {
        headers.insert(X_FORWARDED_HOST, host);
    }
    let mut chain = headers
        .get_all(X_FORWARDED_FOR)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(", ");
    if !chain.is_empty() {
        chain.push_str(", ");
    }
    chain.push_str(&client_ip.to_string());
    if let Ok(value) = HeaderValue::from_str(&chain) {
        headers.insert(X_FORWARDED_FOR, value);
    }
}

async fn splice_upgraded(
    client: OnUpgrade,
    backend: OnUpgrade,
    rec: Option<Recording>,
) -> Result<()> {
    let (client, backend) = tokio::try_join!(client, backend).context("upgrade failed")?;
    let mut client = TokioIo::new(client);
    let mut backend = TokioIo::new(backend);
    let relayed = tokio::io::copy_bidirectional(&mut client, &mut backend).await;
    if let Some(rec) = rec {
        rec.closed();
    }
    match relayed {
        Err(e) if !is_benign_disconnect(&e) => Err(e).context("upgraded connection relay failed"),
        _ => Ok(()),
    }
}

/// Browsers drop idle and half-used connections routinely.
fn is_benign_client_error(e: &hyper::Error) -> bool {
    e.is_incomplete_message()
        || e.is_timeout()
        || std::error::Error::source(e)
            .and_then(|s| s.downcast_ref::<io::Error>())
            .is_some_and(is_benign_disconnect)
}

fn is_benign_disconnect(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handshake_the_client_abandoned_is_not_worth_reporting() {
        let eof = io::Error::new(io::ErrorKind::UnexpectedEof, "tls handshake eof");
        assert_eq!(
            classify_handshake_failure(&eof),
            HandshakeFailure::Abandoned
        );
        let reset = io::Error::from(io::ErrorKind::ConnectionReset);
        assert_eq!(
            classify_handshake_failure(&reset),
            HandshakeFailure::Abandoned
        );
    }

    #[test]
    fn a_refused_certificate_is_told_apart_from_other_failures() {
        let wrap = |e: rustls::Error| io::Error::new(io::ErrorKind::InvalidData, e);
        assert_eq!(
            classify_handshake_failure(&wrap(rustls::Error::AlertReceived(
                rustls::AlertDescription::UnknownCA
            ))),
            HandshakeFailure::Untrusted(rustls::AlertDescription::UnknownCA)
        );
        assert_eq!(
            classify_handshake_failure(&wrap(rustls::Error::General(
                "no server certificate chain resolved".into()
            ))),
            HandshakeFailure::NotOurs
        );
        assert_eq!(
            classify_handshake_failure(&wrap(rustls::Error::AlertReceived(
                rustls::AlertDescription::ProtocolVersion
            ))),
            HandshakeFailure::Other
        );
    }
    use std::collections::HashMap;
    use std::net::SocketAddr;

    use rustls::pki_types::{CertificateDer, ServerName};
    use rustls::{ClientConfig, RootCertStore};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const ROUTED: &str = "cart.default.shop.fghj.internal";

    type TestBody = BoxBody<Bytes, Infallible>;

    fn provider() -> Arc<CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    struct StaticRoutes(HashMap<String, Backend>);

    impl RouteResolver for StaticRoutes {
        fn resolve(&self, host: &str) -> Option<Backend> {
            self.0.get(host).cloned()
        }
    }

    fn routes(entries: &[(&str, &str, u16)]) -> Arc<dyn RouteResolver> {
        Arc::new(StaticRoutes(
            entries
                .iter()
                .map(|(name, host, port)| {
                    let backend = Backend {
                        host: host.to_string(),
                        port: *port,
                        traffic: None,
                    };
                    (name.to_string(), backend)
                })
                .collect(),
        ))
    }

    fn no_routes() -> Arc<dyn RouteResolver> {
        routes(&[])
    }

    fn trusting_client_config(
        ca_der: CertificateDer<'static>,
        provider: Arc<CryptoProvider>,
    ) -> Arc<ClientConfig> {
        let mut roots = RootCertStore::empty();
        roots.add(ca_der).unwrap();
        Arc::new(
            ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }

    /// Answers every request with its own headers, one `name: value` per
    /// line. `/echo` answers with the request body instead, and `/ws` echoes
    /// five bytes back over an upgrade.
    async fn header_echo_backend() -> u16 {
        echo_backend(true).await
    }

    async fn echo_backend(keep_alive: bool) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let service =
                        hyper::service::service_fn(|mut req: Request<Incoming>| async move {
                            if req.uri().path() == "/echo" {
                                let body = req.into_body().collect().await.unwrap().to_bytes();
                                return Ok::<_, Infallible>(Response::new(full(body)));
                            }
                            if req.uri().path() == "/ws"
                                && req.headers().contains_key(header::UPGRADE)
                            {
                                let on_upgrade = hyper::upgrade::on(&mut req);
                                tokio::spawn(async move {
                                    let mut io = TokioIo::new(on_upgrade.await.unwrap());
                                    let mut buf = [0u8; 5];
                                    io.read_exact(&mut buf).await.unwrap();
                                    io.write_all(&buf).await.unwrap();
                                });
                                let mut resp = Response::new(empty());
                                *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
                                let headers = resp.headers_mut();
                                headers.insert(
                                    header::CONNECTION,
                                    HeaderValue::from_static("upgrade"),
                                );
                                headers.insert(header::UPGRADE, HeaderValue::from_static("echo"));
                                return Ok(resp);
                            }
                            let dump: String = req
                                .headers()
                                .iter()
                                .map(|(k, v)| format!("{k}: {}\n", v.to_str().unwrap()))
                                .collect();
                            Ok(Response::new(full(dump)))
                        });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .keep_alive(keep_alive)
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                });
            }
        });
        port
    }

    fn header_line<'a>(dump: &'a str, name: &str) -> Option<&'a str> {
        dump.lines()
            .filter_map(|l| l.split_once(": "))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    }

    async fn https_proxy(
        control_port: u16,
        routes: Arc<dyn RouteResolver>,
    ) -> (SocketAddr, CertificateDer<'static>) {
        let ca = ca::generate_ca_for_tests();
        let ca_der = ca.cert_der_for_tests();
        let resolver = Arc::new(ca::DynamicCertResolver::new(ca, provider(), no_routes()));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_https(
            listener,
            resolver,
            control_port,
            provider(),
            routes,
        ));
        (addr, ca_der)
    }

    async fn http_proxy(routes: Arc<dyn RouteResolver>) -> SocketAddr {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_http_redirect(listener, routes));
        addr
    }

    async fn client_over<S>(io: S) -> SendRequest<TestBody>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
            .await
            .unwrap();
        tokio::spawn(conn.with_upgrades());
        sender
    }

    async fn tls_client(
        addr: SocketAddr,
        sni: &str,
        ca_der: CertificateDer<'static>,
    ) -> SendRequest<TestBody> {
        let connector =
            tokio_rustls::TlsConnector::from(trusting_client_config(ca_der, provider()));
        let tcp = TcpStream::connect(addr).await.unwrap();
        let server_name = ServerName::try_from(sni.to_string()).unwrap();
        let tls = connector
            .connect(server_name, tcp)
            .await
            .expect("handshake with trusted CA must succeed");
        client_over(tls).await
    }

    async fn get(
        sender: &mut SendRequest<TestBody>,
        host: &str,
        extra: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, String) {
        sender.ready().await.unwrap();
        let mut req = Request::builder()
            .uri("/checkout?x=1")
            .header(header::HOST, host);
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        let resp = sender
            .send_request(req.body(Full::default().boxed()).unwrap())
            .await
            .unwrap();
        let (parts, body) = resp.into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        (
            parts.status,
            parts.headers,
            String::from_utf8(body.to_vec()).unwrap(),
        )
    }

    #[tokio::test]
    async fn apex_sni_proxies_to_the_control_backend() {
        let backend_port = header_echo_backend().await;
        let (addr, ca_der) = https_proxy(backend_port, no_routes()).await;

        let mut client = tls_client(addr, dns::ZONE, ca_der).await;
        let (status, _, dump) = get(&mut client, dns::ZONE, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(header_line(&dump, "host"), Some(dns::ZONE));
    }

    #[tokio::test]
    async fn unknown_in_zone_sni_gets_fancy_404() {
        let (addr, ca_der) = https_proxy(0, no_routes()).await;

        let mut client = tls_client(addr, "nonsense.fghj.internal", ca_der).await;
        let (status, _, body) = get(&mut client, "nonsense.fghj.internal", &[]).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("fghj doesn't know this service yet"));
    }

    #[tokio::test]
    async fn every_request_on_a_kept_alive_connection_gets_forwarded_headers() {
        let backend_port = header_echo_backend().await;
        let (addr, ca_der) = https_proxy(0, routes(&[(ROUTED, "127.0.0.1", backend_port)])).await;

        let mut client = tls_client(addr, ROUTED, ca_der).await;
        for _ in 0..2 {
            let (status, _, dump) = get(&mut client, ROUTED, &[]).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(header_line(&dump, "x-forwarded-proto"), Some("https"));
            assert_eq!(header_line(&dump, "x-forwarded-host"), Some(ROUTED));
            assert_eq!(header_line(&dump, "x-forwarded-for"), Some("127.0.0.1"));
        }
    }

    #[tokio::test]
    async fn a_backend_that_closes_between_requests_is_redialed() {
        let backend_port = echo_backend(false).await;
        let (addr, ca_der) = https_proxy(0, routes(&[(ROUTED, "127.0.0.1", backend_port)])).await;

        let mut client = tls_client(addr, ROUTED, ca_der).await;
        for _ in 0..3 {
            let (status, _, _) = get(&mut client, ROUTED, &[]).await;
            assert_eq!(status, StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn forwarded_headers_already_on_the_request_are_kept_and_for_is_appended() {
        let backend_port = header_echo_backend().await;
        let (addr, ca_der) = https_proxy(0, routes(&[(ROUTED, "127.0.0.1", backend_port)])).await;

        let mut client = tls_client(addr, ROUTED, ca_der).await;
        let (_, _, dump) = get(
            &mut client,
            ROUTED,
            &[
                ("x-forwarded-proto", "http"),
                ("x-forwarded-host", "shop.example.com"),
                ("x-forwarded-for", "203.0.113.7"),
            ],
        )
        .await;
        assert_eq!(header_line(&dump, "x-forwarded-proto"), Some("http"));
        assert_eq!(
            header_line(&dump, "x-forwarded-host"),
            Some("shop.example.com")
        );
        assert_eq!(
            header_line(&dump, "x-forwarded-for"),
            Some("203.0.113.7, 127.0.0.1")
        );
    }

    #[tokio::test]
    async fn upgrade_is_spliced_through_to_the_backend() {
        let backend_port = header_echo_backend().await;
        let (addr, ca_der) = https_proxy(0, routes(&[(ROUTED, "127.0.0.1", backend_port)])).await;

        let mut client = tls_client(addr, ROUTED, ca_der).await;
        let req = Request::builder()
            .uri("/ws")
            .header(header::HOST, ROUTED)
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "echo")
            .body(Full::default().boxed())
            .unwrap();
        let resp = client.send_request(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);

        let mut io = TokioIo::new(hyper::upgrade::on(resp).await.unwrap());
        io.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        io.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn unreachable_backend_gets_a_502() {
        let closed = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let closed_port = closed.local_addr().unwrap().port();
        drop(closed);
        let (addr, ca_der) = https_proxy(0, routes(&[(ROUTED, "127.0.0.1", closed_port)])).await;

        let mut client = tls_client(addr, ROUTED, ca_der).await;
        let (status, _, _) = get(&mut client, ROUTED, &[]).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn backend_is_dialed_by_the_host_named_in_backend_not_a_hardcoded_loopback() {
        let backend_port = header_echo_backend().await;
        // `localhost`, so this only passes if `backend.host` is actually dialed.
        let (addr, ca_der) = https_proxy(0, routes(&[(ROUTED, "localhost", backend_port)])).await;

        let mut client = tls_client(addr, ROUTED, ca_der).await;
        let (status, _, _) = get(&mut client, ROUTED, &[]).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn plain_http_additional_host_is_relayed_with_proto_http() {
        let backend_port = header_echo_backend().await;
        let addr = http_proxy(routes(&[("app.example.com", "127.0.0.1", backend_port)])).await;

        let mut client = client_over(TcpStream::connect(addr).await.unwrap()).await;
        let (status, _, dump) = get(&mut client, "app.example.com", &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(header_line(&dump, "x-forwarded-proto"), Some("http"));
        assert_eq!(
            header_line(&dump, "x-forwarded-host"),
            Some("app.example.com")
        );
    }

    #[tokio::test]
    async fn plain_http_in_zone_name_redirects_to_https() {
        let addr = http_proxy(no_routes()).await;

        let mut client = client_over(TcpStream::connect(addr).await.unwrap()).await;
        let (status, headers, _) = get(&mut client, "cart.fghj.internal", &[]).await;
        assert_eq!(status, StatusCode::MOVED_PERMANENTLY);
        assert_eq!(
            headers.get(header::LOCATION).unwrap(),
            "https://cart.fghj.internal/checkout?x=1"
        );
    }

    #[test]
    fn forwarded_for_merges_repeated_lines_and_host_is_skipped_when_absent() {
        let mut headers = HeaderMap::new();
        headers.append(X_FORWARDED_FOR, HeaderValue::from_static("10.0.0.1"));
        headers.append(
            X_FORWARDED_FOR,
            HeaderValue::from_static("10.0.0.2, 10.0.0.3"),
        );
        add_forwarded_headers(&mut headers, "192.168.65.1".parse().unwrap(), "https");

        let chain: Vec<_> = headers.get_all(X_FORWARDED_FOR).iter().collect();
        assert_eq!(chain, ["10.0.0.1, 10.0.0.2, 10.0.0.3, 192.168.65.1"]);
        assert_eq!(headers.get(X_FORWARDED_PROTO).unwrap(), "https");
        assert!(!headers.contains_key(X_FORWARDED_HOST));
    }

    #[test]
    fn hop_by_hop_headers_and_the_ones_connection_names_are_dropped() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, X-Secret"),
        );
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        headers.insert("x-secret", HeaderValue::from_static("1"));
        headers.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        headers.insert(header::TE, HeaderValue::from_static("trailers"));
        headers.insert(header::UPGRADE, HeaderValue::from_static("h2c"));
        headers.insert("x-app", HeaderValue::from_static("kept"));
        strip_hop_by_hop(&mut headers, false);

        let left: Vec<_> = headers.keys().map(|k| k.as_str()).collect();
        assert_eq!(left, ["x-app"]);
    }

    #[test]
    fn an_upgrade_keeps_its_connection_and_upgrade_pair() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("Upgrade, X-Secret"),
        );
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert("x-secret", HeaderValue::from_static("1"));
        headers.insert("sec-websocket-key", HeaderValue::from_static("k"));
        strip_hop_by_hop(&mut headers, true);

        assert_eq!(headers.get(header::CONNECTION).unwrap(), "upgrade");
        assert_eq!(headers.get(header::UPGRADE).unwrap(), "websocket");
        assert_eq!(headers.get("sec-websocket-key").unwrap(), "k");
        assert!(!headers.contains_key("x-secret"));
    }

    async fn post(sender: &mut SendRequest<TestBody>, body: TestBody) -> (StatusCode, Bytes) {
        sender.ready().await.unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/echo")
            .header(header::HOST, ROUTED)
            .body(body)
            .unwrap();
        let resp = sender.send_request(req).await.unwrap();
        let status = resp.status();
        (status, resp.into_body().collect().await.unwrap().to_bytes())
    }

    #[tokio::test]
    async fn request_bodies_reach_the_backend_intact_however_they_are_framed() {
        let backend_port = header_echo_backend().await;
        let (addr, ca_der) = https_proxy(0, routes(&[(ROUTED, "127.0.0.1", backend_port)])).await;
        let mut client = tls_client(addr, ROUTED, ca_der).await;

        // Content-Length, and big enough to span many TLS records.
        let big = Bytes::from((0..8 * 1024 * 1024).map(|i| i as u8).collect::<Vec<_>>());
        let (status, echoed) = post(&mut client, Full::new(big.clone()).boxed()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(echoed, big);

        // No size known up front, so the client sends it chunked.
        let chunks = ["hello", " ", "chunked", " world"]
            .map(|c| Ok::<_, Infallible>(hyper::body::Frame::data(Bytes::from(c))));
        let streamed = http_body_util::StreamBody::new(futures_util::stream::iter(chunks));
        let (status, echoed) = post(&mut client, streamed.boxed()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(echoed, "hello chunked world");

        // And the connection is still good for a plain request afterwards.
        let (status, _, _) = get(&mut client, ROUTED, &[]).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn header_names_reach_the_backend_in_the_case_the_client_sent() {
        let backend = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let backend_port = backend.local_addr().unwrap().port();
        let (head_tx, head_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (sock, _) = backend.accept().await.unwrap();
            let mut sock = tokio::io::BufReader::new(sock);
            let mut head = String::new();
            while !head.ends_with("\r\n\r\n") {
                use tokio::io::AsyncBufReadExt;
                sock.read_line(&mut head).await.unwrap();
            }
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            head_tx.send(head).unwrap();
        });
        let addr = http_proxy(routes(&[("app.example.com", "127.0.0.1", backend_port)])).await;

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: app.example.com\r\nX-Custom-Header: 1\r\n\r\n")
            .await
            .unwrap();
        let head = head_rx.await.unwrap();
        assert!(head.contains("X-Custom-Header: 1\r\n"), "{head}");
    }

    #[tokio::test]
    async fn a_refused_upgrade_leaves_the_connection_usable() {
        let backend_port = header_echo_backend().await;
        let (addr, ca_der) = https_proxy(0, routes(&[(ROUTED, "127.0.0.1", backend_port)])).await;
        let mut client = tls_client(addr, ROUTED, ca_der).await;

        // The echo backend only switches protocols on `/ws`.
        let (status, _, _) = get(
            &mut client,
            ROUTED,
            &[("connection", "upgrade"), ("upgrade", "echo")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _, dump) = get(&mut client, ROUTED, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(header_line(&dump, "x-forwarded-proto"), Some("https"));
    }

    #[tokio::test]
    async fn a_backend_that_silently_drops_an_idle_connection_is_redialed() {
        let backend = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let backend_port = backend.local_addr().unwrap().port();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = backend.accept().await.unwrap();
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut sock = tokio::io::BufReader::new(sock);
                    let mut head = String::new();
                    while !head.ends_with("\r\n\r\n") {
                        use tokio::io::AsyncBufReadExt;
                        if sock.read_line(&mut head).await.unwrap() == 0 {
                            return;
                        }
                    }
                    // Advertises nothing about closing, then closes anyway:
                    // what a backend's idle keep-alive timeout looks like.
                    sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                        .await
                        .unwrap();
                });
            }
        });
        let (addr, ca_der) = https_proxy(0, routes(&[(ROUTED, "127.0.0.1", backend_port)])).await;
        let mut client = tls_client(addr, ROUTED, ca_der).await;

        for _ in 0..3 {
            let (status, _, body) = get(&mut client, ROUTED, &[]).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "ok");
            // Lets the proxy see the close before the next request; racing
            // it is the in-flight case `try_send_request` covers.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    /// Answers the first request on its first connection, then reads the
    /// second one in full and drops the connection without a word: the
    /// idle-timeout race, made deterministic. Later connections answer
    /// everything. Returns the port, the connection count and every request
    /// line it read.
    async fn closes_under_the_second_request_backend() -> (
        u16,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        use std::sync::atomic::Ordering;
        use tokio::io::AsyncBufReadExt;

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (counter, log) = (accepted.clone(), seen.clone());
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let first_connection = counter.fetch_add(1, Ordering::SeqCst) == 0;
                let log = log.clone();
                tokio::spawn(async move {
                    let mut sock = tokio::io::BufReader::new(sock);
                    for n in 0.. {
                        let mut head = String::new();
                        while !head.ends_with("\r\n\r\n") {
                            if sock.read_line(&mut head).await.unwrap_or(0) == 0 {
                                return;
                            }
                        }
                        let length = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(str::to_string)
                            })
                            .map_or(0, |v| v.trim().parse().unwrap());
                        let mut body = vec![0u8; length];
                        sock.read_exact(&mut body).await.unwrap();
                        log.lock()
                            .unwrap()
                            .push(head.lines().next().unwrap().to_string());
                        if first_connection && n == 1 {
                            return;
                        }
                        sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                            .await
                            .unwrap();
                    }
                });
            }
        });
        (port, accepted, seen)
    }

    #[tokio::test]
    async fn an_idempotent_request_lost_to_a_dying_connection_is_sent_again() {
        let (backend_port, accepted, seen) = closes_under_the_second_request_backend().await;
        let (addr, ca_der) = https_proxy(0, routes(&[(ROUTED, "127.0.0.1", backend_port)])).await;
        let mut client = tls_client(addr, ROUTED, ca_der).await;

        for _ in 0..2 {
            let (status, _, body) = get(&mut client, ROUTED, &[]).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "ok");
        }
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(seen.lock().unwrap().len(), 3, "the lost GET went out twice");
    }

    #[tokio::test]
    async fn a_request_with_a_body_lost_to_a_dying_connection_is_not_sent_again() {
        let (backend_port, accepted, seen) = closes_under_the_second_request_backend().await;
        let (addr, ca_der) = https_proxy(0, routes(&[(ROUTED, "127.0.0.1", backend_port)])).await;
        let mut client = tls_client(addr, ROUTED, ca_der).await;

        let (status, _, _) = get(&mut client, ROUTED, &[]).await;
        assert_eq!(status, StatusCode::OK);
        // Bounded: a wrongly replayed copy keeps `Content-Length` without the
        // body, and the backend would wait for it forever.
        let (status, _) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            post(&mut client, Full::new(Bytes::from("charge card")).boxed()),
        )
        .await
        .expect("the POST must fail fast, not be replayed");
        assert_eq!(status, StatusCode::BAD_GATEWAY);

        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            *seen.lock().unwrap(),
            ["GET /checkout?x=1 HTTP/1.1", "POST /echo HTTP/1.1"]
        );
    }

    #[tokio::test]
    async fn plain_http_unrouted_host_redirects_to_https() {
        let addr = http_proxy(no_routes()).await;

        let mut client = client_over(TcpStream::connect(addr).await.unwrap()).await;
        let (status, headers, _) = get(&mut client, "example.com", &[]).await;
        assert_eq!(status, StatusCode::MOVED_PERMANENTLY);
        assert_eq!(
            headers.get(header::LOCATION).unwrap(),
            "https://example.com/checkout?x=1"
        );
    }

    #[tokio::test]
    async fn client_that_does_not_trust_our_ca_fails_the_handshake() {
        let (addr, _) = https_proxy(0, no_routes()).await;

        let other_ca_der = ca::generate_ca_for_tests().cert_der_for_tests();
        let connector =
            tokio_rustls::TlsConnector::from(trusting_client_config(other_ca_der, provider()));
        let tcp = TcpStream::connect(addr).await.unwrap();
        let server_name = ServerName::try_from(dns::ZONE.to_string()).unwrap();
        let result = connector.connect(server_name, tcp).await;
        assert!(
            result.is_err(),
            "a client trusting an unrelated CA must not accept this handshake"
        );
    }

    #[test]
    fn benign_disconnect_kinds_are_recognized() {
        assert!(is_benign_disconnect(&io::Error::from(
            io::ErrorKind::ConnectionReset
        )));
        assert!(is_benign_disconnect(&io::Error::from(
            io::ErrorKind::BrokenPipe
        )));
        assert!(is_benign_disconnect(&io::Error::from(
            io::ErrorKind::UnexpectedEof
        )));
        assert!(!is_benign_disconnect(&io::Error::from(
            io::ErrorKind::TimedOut
        )));
        assert!(!is_benign_disconnect(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    fn recorded_routes(port: u16, log: &Arc<TrafficLog>) -> Arc<dyn RouteResolver> {
        let backend = Backend {
            host: "127.0.0.1".to_string(),
            port,
            traffic: Some(log.clone()),
        };
        Arc::new(StaticRoutes(HashMap::from([(ROUTED.to_string(), backend)])))
    }

    /// The proxy finishes a recording when hyper is done with the body,
    /// which can be a moment after the client has read it.
    async fn settled(log: &TrafficLog, id: u64) -> serde_json::Value {
        for _ in 0..100 {
            let v = serde_json::to_value(log.get(id).unwrap().detail()).unwrap();
            if !["pending", "streaming", "open"].contains(&v["state"].as_str().unwrap()) {
                return v;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("exchange {id} never settled");
    }

    #[tokio::test]
    async fn recording_switched_on_mid_connection_captures_the_next_request() {
        let backend_port = header_echo_backend().await;
        let store = crate::web::traffic::TrafficStore::default();
        let log = store.log("ws", "cart");
        let (addr, ca_der) = https_proxy(0, recorded_routes(backend_port, &log)).await;
        let mut client = tls_client(addr, ROUTED, ca_der).await;

        let (status, _) = post(
            &mut client,
            Full::new(Bytes::from_static(b"unrecorded")).boxed(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(log.summaries().is_empty());

        log.set_mode(Some(crate::web::traffic::BodyMode::Text));
        let (status, body) = post(
            &mut client,
            Full::new(Bytes::from_static(b"{\"a\":1}")).boxed(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&body[..], b"{\"a\":1}");

        let summaries = log.summaries();
        assert_eq!(summaries.len(), 1);
        let v = settled(&log, summaries[0].id).await;
        assert_eq!(v["method"], "POST");
        assert_eq!(v["url"], "/echo");
        assert_eq!(v["host"], ROUTED);
        assert_eq!(v["source"], "host");
        assert_eq!(v["scheme"], "https");
        assert_eq!(v["status"], 200);
        assert_eq!(v["state"], "done");
        // Untyped both ways, so kept because they sniff as text.
        assert_eq!(v["request"]["body"]["text"], "{\"a\":1}");
        assert_eq!(v["response"]["body"]["text"], "{\"a\":1}");
        // Headers as the client sent them, before the proxy added its own.
        let request_headers = v["request"]["headers"].as_array().unwrap();
        assert!(!request_headers.iter().any(|h| h[0] == "x-forwarded-proto"));
    }

    #[tokio::test]
    async fn a_502_is_recorded_with_its_error() {
        let closed = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let closed_port = closed.local_addr().unwrap().port();
        drop(closed);
        let store = crate::web::traffic::TrafficStore::default();
        let log = store.log("ws", "cart");
        log.set_mode(Some(crate::web::traffic::BodyMode::Text));
        let (addr, ca_der) = https_proxy(0, recorded_routes(closed_port, &log)).await;

        let mut client = tls_client(addr, ROUTED, ca_der).await;
        let (status, _, _) = get(&mut client, ROUTED, &[]).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);

        let v = settled(&log, log.summaries()[0].id).await;
        assert_eq!(v["status"], 502);
        assert_eq!(v["state"], "error");
        assert!(
            v["error"].as_str().unwrap().contains("failed to connect"),
            "{v}"
        );
    }

    #[tokio::test]
    async fn an_upgrade_is_recorded_until_it_closes() {
        let backend_port = header_echo_backend().await;
        let store = crate::web::traffic::TrafficStore::default();
        let log = store.log("ws", "cart");
        log.set_mode(Some(crate::web::traffic::BodyMode::Text));
        let (addr, ca_der) = https_proxy(0, recorded_routes(backend_port, &log)).await;
        let mut client = tls_client(addr, ROUTED, ca_der).await;

        client.ready().await.unwrap();
        let req = Request::builder()
            .uri("/ws")
            .header(header::HOST, ROUTED)
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "echo")
            .body(Full::default().boxed())
            .unwrap();
        let resp = client.send_request(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);
        let mut io = TokioIo::new(hyper::upgrade::on(resp).await.unwrap());
        io.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        io.read_exact(&mut buf).await.unwrap();

        let id = log.summaries()[0].id;
        assert_eq!(log.get(id).unwrap().summary().state, "open");
        drop(io);
        let v = settled(&log, id).await;
        assert_eq!(v["state"], "closed");
        assert_eq!(v["upgraded"], true);
        assert_eq!(v["status"], 101);
    }
}
