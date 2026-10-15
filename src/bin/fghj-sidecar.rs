//! The per-run in-network sidecar: TLS proxy and DNS authority for the run's
//! docker network, so containers reach siblings by the same names a browser
//! uses. A separate binary because it holds the signing key. Paths are fixed;
//! only `orchestrator/`'s bind mounts have to agree with them.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use fghj::dns;
use fghj::supervisor;
use fghj::web::traffic::TrafficStore;
use fghj::web::{ca, capture, proxy};
use serde::Deserialize;
use tokio::net::{TcpListener, UdpSocket};

const CA_DIR: &str = "/etc/fghj-sidecar/ca";
const ROUTES_DIR: &str = "/etc/fghj-sidecar/routes";
const ROUTES_PATH: &str = "/etc/fghj-sidecar/routes/routes.json";
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Mirrors `orchestrator`'s `RouteFileEntry` without depending on it.
/// `connect_host` is the backend's raw-zone alias, which Docker's DNS resolves.
#[derive(Debug, Clone, Deserialize)]
struct RouteFileEntry {
    lookup: String,
    #[serde(default)]
    wildcard: bool,
    connect_host: String,
    connect_port: u16,
    /// Whose log a request to this route is recorded in.
    #[serde(default)]
    node_id: String,
}

/// Polled, not inotify: bind-mount events across the VM boundary are
/// unreliable.
struct FileRoutes {
    own_ip: Ipv4Addr,
    routes: Mutex<Vec<RouteFileEntry>>,
    traffic: Arc<TrafficStore>,
}

impl FileRoutes {
    fn load() -> Vec<RouteFileEntry> {
        let Ok(bytes) = std::fs::read(ROUTES_PATH) else {
            return Vec::new();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    fn spawn_polling(own_ip: Ipv4Addr, traffic: Arc<TrafficStore>) -> Arc<Self> {
        let this = Arc::new(Self {
            own_ip,
            routes: Mutex::new(Self::load()),
            traffic,
        });
        supervisor::supervise_forever("routes file watcher", Self::poll_routes(this.clone()));
        this
    }

    /// A `loop` that never breaks would type as `!`, which no `supervisor::Outcome`
    /// covers.
    async fn poll_routes(watched: Arc<Self>) {
        let mut last_mtime: Option<SystemTime> = std::fs::metadata(ROUTES_PATH)
            .ok()
            .and_then(|m| m.modified().ok());
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            let mtime = std::fs::metadata(ROUTES_PATH)
                .ok()
                .and_then(|m| m.modified().ok());
            if mtime != last_mtime {
                last_mtime = mtime;
                *watched.routes.lock().unwrap() = Self::load();
            }
        }
    }
}

impl proxy::RouteResolver for FileRoutes {
    fn resolve(&self, host: &str) -> Option<proxy::Backend> {
        let routes = self.routes.lock().unwrap();
        // Exact matches win, as in `state::query::resolve_route`.
        routes
            .iter()
            .find(|r| !r.wildcard && r.lookup == host)
            .or_else(|| {
                routes.iter().find(|r| {
                    r.wildcard && (host == r.lookup || host.ends_with(&format!(".{}", r.lookup)))
                })
            })
            .map(|r| proxy::Backend {
                host: r.connect_host.clone(),
                port: r.connect_port,
                traffic: (!r.node_id.is_empty())
                    .then(|| self.traffic.log(capture::SIDECAR_WORKSPACE, &r.node_id)),
            })
    }
}

/// A name is ours exactly when it's routable.
impl dns::ZoneSource for FileRoutes {
    fn answer_for(&self, qname: &str) -> Option<Ipv4Addr> {
        proxy::RouteResolver::resolve(self, qname).map(|_| self.own_ip)
    }
}

/// Connecting a UDP socket sends nothing but makes the kernel pick our source
/// address.
fn discover_own_ip() -> Result<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")
        .context("failed to bind a scratch UDP socket for self-IP discovery")?;
    socket
        .connect("10.255.255.255:1")
        .context("failed to route a scratch UDP socket for self-IP discovery")?;
    match socket
        .local_addr()
        .context("failed to read local address after connect")?
        .ip()
    {
        IpAddr::V4(ip) => Ok(ip),
        IpAddr::V6(ip) => bail!("expected an IPv4 local address, got {ip}"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let ca = ca::ensure_ca(Path::new(CA_DIR))
        .context("failed to load CA from the mounted /etc/fghj-sidecar/ca")?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let own_ip = discover_own_ip().context("failed to discover this sidecar's own IP")?;
    let traffic = Arc::new(TrafficStore::default());
    let file_routes = FileRoutes::spawn_polling(own_ip, traffic.clone());
    let routes: Arc<dyn proxy::RouteResolver> = file_routes.clone();
    let zones: Arc<dyn dns::ZoneSource> = file_routes;
    let cert_resolver = Arc::new(ca::DynamicCertResolver::new(
        ca,
        provider.clone(),
        routes.clone(),
    ));

    let http_listener = TcpListener::bind(("0.0.0.0", proxy::HTTP_PORT))
        .await
        .context("failed to bind sidecar HTTP listener")?;
    let https_listener = TcpListener::bind(("0.0.0.0", proxy::HTTPS_PORT))
        .await
        .context("failed to bind sidecar HTTPS listener")?;

    println!(
        "fghj-sidecar: listening on 0.0.0.0:{}/{}",
        proxy::HTTP_PORT,
        proxy::HTTPS_PORT
    );

    // Every node's `--dns` points here; unrecognized names go to Docker's
    // embedded resolver.
    let dns_socket = UdpSocket::bind(("0.0.0.0", 53))
        .await
        .context("failed to bind sidecar DNS listener on 0.0.0.0:53")?;
    let docker_embedded_dns = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 11), 53));

    let capture_listener = TcpListener::bind(("0.0.0.0", capture::CAPTURE_PORT))
        .await
        .context("failed to bind sidecar capture listener")?;
    println!("fghj-sidecar: DNS listening on 0.0.0.0:53, answering recognized names with {own_ip}");

    // Never dialed: nothing in a run's network presents the zone apex.
    let control_port = 0;
    tokio::join!(
        proxy::serve_http_redirect(http_listener, routes.clone()),
        proxy::serve_https(
            https_listener,
            cert_resolver,
            control_port,
            provider,
            routes
        ),
        dns::serve(dns_socket, zones, Some(docker_embedded_dns)),
        capture::serve(
            capture_listener,
            traffic,
            Path::new(ROUTES_DIR).join(capture::TOKEN_FILE)
        ),
    );
    Ok(())
}
