use std::io;
use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::crypto::CryptoProvider;
use rustls::ServerConfig;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

use crate::{ca, dns};

/// Resolves an in-zone hostname that isn't the zone apex to the `127.0.0.1`
/// port of the running container it should be proxied to, if any. Kept as a
/// trait (implemented by `daemon::WorkspaceRegistry::resolve_route` in
/// production) rather than a concrete dependency so this module doesn't need
/// to know anything about workspaces, runs, or Docker — and so tests can
/// exercise dispatch with a plain in-memory map instead of real containers.
pub trait RouteResolver: Send + Sync {
    fn resolve(&self, host: &str) -> Option<u16>;
}

/// `fghj` occupies these unconditionally while `fghjd` runs — unlike the
/// DNS/control-API ports, these can't be ephemeral: a bare
/// `https://fghj.internal` URL with no `:port` only ever reaches port 443.
pub const HTTP_PORT: u16 = 80;
pub const HTTPS_PORT: u16 = 443;

/// Bound to `127.0.0.1` only, matching every other `fghjd` listener — DNS
/// only ever answers `*.fghj.internal` with `127.0.0.1`, so there's no
/// reason to expose this proxy to the network.
pub async fn bind_http() -> Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", HTTP_PORT))
        .await
        .with_context(|| format!("failed to bind fghj's HTTP listener on port {HTTP_PORT} — is something else already using it?"))
}

pub async fn bind_https() -> Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", HTTPS_PORT))
        .await
        .with_context(|| format!("failed to bind fghj's HTTPS listener on port {HTTPS_PORT} — is something else already using it?"))
}

/// Longest request line / header line this server will read before giving
/// up — plenty for a bare `GET /path HTTP/1.1` and a `Host:` header, and
/// small enough that a client that never sends `\r\n` can't hold a
/// connection's buffer open indefinitely.
const MAX_LINE_LEN: usize = 8 * 1024;

fn parse_request_path(request_line: &str) -> Option<&str> {
    request_line.trim_end().split(' ').nth(1)
}

fn parse_host_header(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("Host:").or_else(|| line.strip_prefix("host:"))?;
    Some(rest.trim())
}

fn build_redirect_response(host: &str, path: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 301 Moved Permanently\r\nLocation: https://{host}{path}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
}

/// Serves forever on `listener` (port 80): reads just enough of each plain
/// HTTP request to learn its `Host` and path, then redirects to the same
/// URL over HTTPS. A small hand-rolled reader is enough here — same spirit
/// as `dns::parse_query` — since all this needs is the request line and one
/// header, not a general-purpose HTTP implementation.
pub async fn serve_http_redirect(listener: TcpListener) {
    println!("fghjd: HTTP listener on 127.0.0.1:{HTTP_PORT}, redirecting everything to https://");
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("fghjd: HTTP accept error: {e}");
                continue;
            }
        };
        tokio::spawn(async move {
            if let Err(e) = handle_http_redirect(stream).await {
                eprintln!("fghjd: HTTP redirect handler error: {e}");
            }
        });
    }
}

async fn handle_http_redirect(stream: TcpStream) -> Result<()> {
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    read_bounded_line(&mut reader, &mut request_line).await?;
    let path = parse_request_path(&request_line).unwrap_or("/").to_string();

    let mut host = String::new();
    loop {
        let mut line = String::new();
        let n = read_bounded_line(&mut reader, &mut line).await?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(h) = parse_host_header(&line) {
            host = h.to_string();
        }
    }

    let response = build_redirect_response(&host, &path);
    let mut stream = reader.into_inner();
    stream.write_all(&response).await?;
    Ok(())
}

/// `read_line`, but bailing out instead of growing `buf` without bound if
/// the peer never sends a newline.
async fn read_bounded_line<R: AsyncBufReadExt + Unpin>(reader: &mut R, buf: &mut String) -> Result<usize> {
    let n = reader.read_line(buf).await.context("failed to read from client")?;
    if buf.len() > MAX_LINE_LEN {
        anyhow::bail!("client sent a line longer than {MAX_LINE_LEN} bytes");
    }
    Ok(n)
}

fn not_found_response() -> Vec<u8> {
    let body = br##"<!doctype html>
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
    let mut resp = format!(
        "HTTP/1.1 404 Not Found\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(body);
    resp
}

/// Serves forever on `listener` (port 443): terminates TLS using
/// `cert_resolver` (which also rejects any out-of-zone SNI at the handshake
/// layer — see `ca::DynamicCertResolver`), then routes by the negotiated
/// server name. The zone apex (`fghj.internal`) always goes to
/// `127.0.0.1:{control_port}`, i.e. `fghjd`'s own control API/UI; any other
/// in-zone name is looked up via `routes` (backed by
/// `daemon::WorkspaceRegistry::resolve_route` in production) and proxied to
/// that container's published port if a running one claims it. Anything else
/// gets a styled 404 instead of a browser TLS error, since it already has a
/// valid certificate for its name.
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

    println!("fghjd: HTTPS listener on 127.0.0.1:{HTTPS_PORT}, routing https://{} to the control API", dns::ZONE);
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("fghjd: HTTPS accept error: {e}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let routes = routes.clone();
        tokio::spawn(async move {
            match acceptor.accept(stream).await {
                Ok(tls_stream) => handle_https_connection(tls_stream, control_port, routes).await,
                // Expected and frequent for out-of-zone SNI, which
                // `cert_resolver` intentionally aborts the handshake for.
                Err(e) => eprintln!("fghjd: TLS handshake error: {e}"),
            }
        });
    }
}

async fn handle_https_connection(mut tls_stream: TlsStream<TcpStream>, control_port: u16, routes: Arc<dyn RouteResolver>) {
    let server_name = {
        let (_, conn) = tls_stream.get_ref();
        conn.server_name().map(|s| s.to_string())
    };
    // `cert_resolver` only ever completes a handshake for an in-zone name,
    // so `server_name` being `None` here would mean rustls accepted a
    // connection without SNI at all — not possible given how the resolver
    // is wired, but handled explicitly rather than assumed.
    let Some(name) = server_name else {
        return;
    };

    let backend_port = if name == dns::ZONE { Some(control_port) } else { routes.resolve(&name) };

    match backend_port {
        Some(port) => {
            if let Err(e) = relay_to_backend(&mut tls_stream, port).await {
                eprintln!("fghjd: backend proxy error ({name}): {e}");
            }
        }
        None => {
            let _ = tls_stream.write_all(&not_found_response()).await;
            // Sends a proper TLS close_notify rather than just dropping the
            // socket — otherwise well-behaved TLS clients (rustls included)
            // treat the abrupt EOF as a truncation error instead of a clean end
            // of response.
            let _ = tls_stream.shutdown().await;
        }
    }
}

/// Relays `tls_stream` to whatever is listening on `127.0.0.1:port` — the
/// control API for the zone apex, or a running container's published port
/// for everything else `routes` recognizes.
async fn relay_to_backend(tls_stream: &mut TlsStream<TcpStream>, port: u16) -> Result<()> {
    let mut backend = TcpStream::connect(("127.0.0.1", port))
        .await
        .with_context(|| format!("failed to connect to backend on 127.0.0.1:{port}"))?;

    // Split into two independently-tracked copy directions (instead of one
    // `copy_bidirectional`) so an error can be attributed to a side: the
    // client (browser) is inherently noisy — it opens spare/speculative
    // connections and resets them, or a tab gets closed mid-request — while
    // the backend is our own control API or a locally-run container, which
    // should always be reachable and well-behaved once connected. This isn't perfectly
    // precise (a client disappearing mid-download still surfaces as a write
    // error on the backend -> client leg) but it's a much better signal than
    // treating every reset/broken-pipe/EOF the same regardless of cause.
    let (mut client_read, mut client_write) = tokio::io::split(tls_stream);
    let (mut backend_read, mut backend_write) = backend.split();

    let client_to_backend = async {
        let result = tokio::io::copy(&mut client_read, &mut backend_write).await;
        let _ = backend_write.shutdown().await;
        result
    };
    let backend_to_client = async {
        let result = tokio::io::copy(&mut backend_read, &mut client_write).await;
        let _ = client_write.shutdown().await;
        result
    };
    let (c2b, b2c) = tokio::join!(client_to_backend, backend_to_client);

    if let Err(e) = c2b
        && !is_benign_disconnect(&e)
    {
        return Err(e).context("proxy relay (client -> backend) failed");
    }
    if let Err(e) = b2c {
        return Err(e).context("proxy relay (backend -> client) failed");
    }

    Ok(())
}

/// Whether `e` is the kind of IO error a client (browser) routinely produces
/// by resetting a spare/speculative connection or closing a tab mid-request —
/// not a sign of an actual proxy or backend problem.
fn is_benign_disconnect(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::ServerName;
    use rustls::{ClientConfig, RootCertStore};
    use tokio::io::AsyncReadExt;

    #[test]
    fn parses_request_line_and_host_header() {
        assert_eq!(parse_request_path("GET /cart HTTP/1.1\r\n"), Some("/cart"));
        assert_eq!(parse_request_path("GET / HTTP/1.1\r\n"), Some("/"));
        assert_eq!(parse_host_header("Host: cart.fghj.internal\r\n"), Some("cart.fghj.internal"));
        assert_eq!(parse_host_header("host: cart.fghj.internal\r\n"), Some("cart.fghj.internal"));
        assert_eq!(parse_host_header("Content-Type: text/html\r\n"), None);
    }

    #[test]
    fn builds_expected_redirect_bytes() {
        let resp = build_redirect_response("cart.fghj.internal", "/checkout");
        let text = String::from_utf8(resp).unwrap();
        assert!(text.starts_with("HTTP/1.1 301 Moved Permanently\r\n"));
        assert!(text.contains("Location: https://cart.fghj.internal/checkout\r\n"));
    }

    fn provider() -> Arc<CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    /// A `RouteResolver` backed by a plain in-memory map, so dispatch logic
    /// can be exercised without a real workspace/run/Docker stack.
    struct StaticRoutes(std::collections::HashMap<String, u16>);

    impl RouteResolver for StaticRoutes {
        fn resolve(&self, host: &str) -> Option<u16> {
            self.0.get(host).copied()
        }
    }

    fn no_routes() -> Arc<dyn RouteResolver> {
        Arc::new(StaticRoutes(std::collections::HashMap::new()))
    }

    fn trusting_client_config(ca_der: rustls::pki_types::CertificateDer<'static>, provider: Arc<CryptoProvider>) -> Arc<ClientConfig> {
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

    /// End-to-end: a real TLS client, trusting the test CA, connects over a
    /// real loopback socket, requests the apex SNI, and the proxy relays it
    /// to a dummy backend that echoes back whatever it received.
    #[tokio::test]
    async fn apex_sni_proxies_to_the_control_backend() {
        let ca = ca::generate_ca_for_tests();
        let ca_der = ca.cert_der_for_tests();
        let resolver = Arc::new(ca::DynamicCertResolver::new(ca, provider()));

        let backend = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let backend_port = backend.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = backend.accept().await.unwrap();
            let mut buf = [0u8; 5];
            sock.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"hello");
            sock.write_all(b"world").await.unwrap();
        });

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_https(listener, resolver, backend_port, provider(), no_routes()));

        let client_config = trusting_client_config(ca_der, provider());
        let connector = tokio_rustls::TlsConnector::from(client_config);
        let tcp = TcpStream::connect(addr).await.unwrap();
        let server_name = ServerName::try_from(dns::ZONE.to_string()).unwrap();
        let mut tls = connector.connect(server_name, tcp).await.expect("handshake with trusted CA must succeed");

        tls.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        tls.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"world");
    }

    #[tokio::test]
    async fn unknown_in_zone_sni_gets_fancy_404() {
        let ca = ca::generate_ca_for_tests();
        let ca_der = ca.cert_der_for_tests();
        let resolver = Arc::new(ca::DynamicCertResolver::new(ca, provider()));

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        // control_port is never dialed for a non-apex name, so 0 is fine here.
        tokio::spawn(serve_https(listener, resolver, 0, provider(), no_routes()));

        let client_config = trusting_client_config(ca_der, provider());
        let connector = tokio_rustls::TlsConnector::from(client_config);
        let tcp = TcpStream::connect(addr).await.unwrap();
        let server_name = ServerName::try_from("nonsense.fghj.internal".to_string()).unwrap();
        let mut tls = connector.connect(server_name, tcp).await.expect("in-zone name must still get a certificate");

        let mut buf = Vec::new();
        tls.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf);
        assert!(text.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(text.contains("fghj doesn't know this service yet"));
    }

    /// A non-apex, in-zone SNI that a `RouteResolver` recognizes must be
    /// proxied to that route's backend, exactly like the zone apex is —
    /// the real per-service dispatch path this session wires up.
    #[tokio::test]
    async fn routed_in_zone_sni_proxies_to_its_registered_backend() {
        let ca = ca::generate_ca_for_tests();
        let ca_der = ca.cert_der_for_tests();
        let resolver = Arc::new(ca::DynamicCertResolver::new(ca, provider()));

        let backend = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let backend_port = backend.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = backend.accept().await.unwrap();
            let mut buf = [0u8; 5];
            sock.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"hello");
            sock.write_all(b"world").await.unwrap();
        });

        let mut map = std::collections::HashMap::new();
        map.insert("cart.default.shop.fghj.internal".to_string(), backend_port);
        let routes: Arc<dyn RouteResolver> = Arc::new(StaticRoutes(map));

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        // control_port is never dialed for a routed non-apex name, so 0 is fine here.
        tokio::spawn(serve_https(listener, resolver, 0, provider(), routes));

        let client_config = trusting_client_config(ca_der, provider());
        let connector = tokio_rustls::TlsConnector::from(client_config);
        let tcp = TcpStream::connect(addr).await.unwrap();
        let server_name = ServerName::try_from("cart.default.shop.fghj.internal".to_string()).unwrap();
        let mut tls = connector.connect(server_name, tcp).await.expect("handshake with trusted CA must succeed");

        tls.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        tls.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"world");
    }

    #[tokio::test]
    async fn client_that_does_not_trust_our_ca_fails_the_handshake() {
        let ca = ca::generate_ca_for_tests();
        let resolver = Arc::new(ca::DynamicCertResolver::new(ca, provider()));

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_https(listener, resolver, 0, provider(), no_routes()));

        // A root store with a *different* CA — this client must not accept
        // the certificate our proxy presents.
        let other_ca = ca::generate_ca_for_tests();
        let other_ca_der = other_ca.cert_der_for_tests();
        let client_config = trusting_client_config(other_ca_der, provider());
        let connector = tokio_rustls::TlsConnector::from(client_config);
        let tcp = TcpStream::connect(addr).await.unwrap();
        let server_name = ServerName::try_from(dns::ZONE.to_string()).unwrap();
        let result = connector.connect(server_name, tcp).await;
        assert!(result.is_err(), "a client trusting an unrelated CA must not accept this handshake");
    }

    #[test]
    fn benign_disconnect_kinds_are_recognized() {
        assert!(is_benign_disconnect(&io::Error::from(io::ErrorKind::ConnectionReset)));
        assert!(is_benign_disconnect(&io::Error::from(io::ErrorKind::BrokenPipe)));
        assert!(is_benign_disconnect(&io::Error::from(io::ErrorKind::UnexpectedEof)));
        assert!(!is_benign_disconnect(&io::Error::from(io::ErrorKind::TimedOut)));
        assert!(!is_benign_disconnect(&io::Error::from(io::ErrorKind::PermissionDenied)));
    }

    /// Sets up a real TLS handshake over a loopback socket and hands back
    /// both ends: the server-side stream `relay_to_backend` operates on, and
    /// the client-side stream the test uses to simulate a well-behaved or
    /// abruptly-reset browser connection.
    async fn handshake_pair(resolver: Arc<ca::DynamicCertResolver>, ca_der: rustls::pki_types::CertificateDer<'static>) -> (TlsStream<TcpStream>, tokio_rustls::client::TlsStream<TcpStream>) {
        let server_config = Arc::new(
            ServerConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_cert_resolver(resolver),
        );
        let acceptor = TlsAcceptor::from(server_config);

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            acceptor.accept(tcp).await.unwrap()
        });

        let client_config = trusting_client_config(ca_der, provider());
        let connector = tokio_rustls::TlsConnector::from(client_config);
        let tcp = TcpStream::connect(addr).await.unwrap();
        let server_name = ServerName::try_from(dns::ZONE.to_string()).unwrap();
        let client_tls = connector.connect(server_name, tcp).await.unwrap();

        let server_tls = server_task.await.unwrap();
        (server_tls, client_tls)
    }

    #[tokio::test]
    async fn client_side_reset_is_not_reported_as_a_relay_failure() {
        let ca = ca::generate_ca_for_tests();
        let ca_der = ca.cert_der_for_tests();
        let resolver = Arc::new(ca::DynamicCertResolver::new(ca, provider()));
        let (mut server_tls, client_tls) = handshake_pair(resolver, ca_der).await;

        // Force a hard reset (RST) instead of a graceful close, mirroring a
        // browser abandoning a spare/speculative connection mid-relay.
        // `set_linger` is deprecated because a *nonzero* linger can block a
        // runtime thread on drop — doesn't apply here, a zero linger closes
        // immediately (that's precisely what forces the RST).
        let (client_tcp, _) = client_tls.get_ref();
        #[allow(deprecated)]
        client_tcp.set_linger(Some(std::time::Duration::ZERO)).unwrap();
        drop(client_tls);

        let backend = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let backend_port = backend.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (sock, _) = backend.accept().await.unwrap();
            drop(sock);
        });

        let result = relay_to_backend(&mut server_tls, backend_port).await;
        assert!(result.is_ok(), "a client-side reset must not be reported as a relay failure: {result:?}");
    }

    #[tokio::test]
    async fn backend_side_reset_is_reported_as_a_relay_failure() {
        let ca = ca::generate_ca_for_tests();
        let ca_der = ca.cert_der_for_tests();
        let resolver = Arc::new(ca::DynamicCertResolver::new(ca, provider()));
        let (mut server_tls, client_tls) = handshake_pair(resolver, ca_der).await;

        // Clean close on the client side, so the client -> backend leg sees
        // an ordinary EOF rather than an error — isolating the backend as
        // the only source of trouble in this relay.
        drop(client_tls);

        let backend = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let backend_port = backend.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (sock, _) = backend.accept().await.unwrap();
            // Force a hard reset, mirroring the control API dying mid-relay
            // (crash, panic, forcibly closed) rather than closing cleanly.
            #[allow(deprecated)]
            sock.set_linger(Some(std::time::Duration::ZERO)).unwrap();
            drop(sock);
        });

        let result = relay_to_backend(&mut server_tls, backend_port).await;
        assert!(result.is_err(), "a backend-side reset must be reported as a relay failure, not silently swallowed");
    }
}
