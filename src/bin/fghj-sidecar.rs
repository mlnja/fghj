//! `fghj`'s in-network TLS proxy sidecar — one per run, attached to that
//! run's own docker network, so a container inside the network can reach a
//! sibling's `*.fghj.internal` (or `#AdditionalHost`) name over HTTPS with
//! the exact same addressing a browser outside the network uses against the
//! host-side proxy (`fghjd::proxy`/`fghjd::ca`, reused here unmodified). Also
//! this run's in-network DNS authority for that same zone (see `dns::serve`
//! below) — every node points `--dns` here first, so no per-consumer opt-in
//! is needed to reach either the proxy or a sibling's real, unproxied
//! `*.fghj.raw.internal` address.
//!
//! A separate binary rather than a mode flag on `fghjd`: this container gets
//! the CA's private key bind-mounted in (read-only), so it deserves its own
//! minimal, easy-to-audit entrypoint rather than a branch inside the daemon
//! binary, which also unconditionally requires root and runs the full
//! control API. See `runs.rs`'s sidecar lifecycle for how this gets started,
//! and `sidecar_image.rs` for how its Docker image gets built.
//!
//! Fixed, hardcoded mount paths (no CLI args, nothing to configure) — the
//! bind mounts `runs.rs` sets up are the only thing that ever has to agree
//! with this binary about paths.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use fghj::{ca, dns, proxy};
use serde::Deserialize;
use tokio::net::{TcpListener, UdpSocket};

const CA_DIR: &str = "/etc/fghj-sidecar/ca";
const ROUTES_PATH: &str = "/etc/fghj-sidecar/routes/routes.json";
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// One entry from the route table `runs.rs` writes — see its
/// `RouteFileEntry` (kept as a separately-defined, field-name-matching type
/// rather than a shared one, so this binary doesn't need to depend on
/// `fghj::runs` at all). `lookup` is a `fghj.internal` domain (or an active
/// alias) this sidecar is the DNS authority for, per every node's `--dns`
/// pointing here first; `connect_host`/`connect_port` is where to actually
/// relay it — the backend's own `fghj.raw.internal` domain, a Docker network
/// alias Docker's own embedded DNS already resolves for any container on
/// this network, including this one.
#[derive(Debug, Clone, Deserialize)]
struct RouteFileEntry {
    lookup: String,
    #[serde(default)]
    wildcard: bool,
    connect_host: String,
    connect_port: u16,
}

/// Polling (not inotify) `RouteResolver` — bind-mount filesystem event
/// propagation across the OrbStack/Docker-Desktop virtualization boundary is
/// unreliable, exactly the kind of platform-specific gap this whole feature
/// exists to avoid. A 1s poll of one small JSON file costs nothing and has
/// no missed-event failure mode.
struct FileRoutes {
    /// This sidecar container's own in-network address — every recognized
    /// query is answered with this, never with a per-backend IP (see
    /// `dns::ZoneSource`'s impl below).
    own_ip: Ipv4Addr,
    routes: Mutex<Vec<RouteFileEntry>>,
}

impl FileRoutes {
    fn load() -> Vec<RouteFileEntry> {
        let Ok(bytes) = std::fs::read(ROUTES_PATH) else {
            return Vec::new();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    fn spawn_polling(own_ip: Ipv4Addr) -> Arc<Self> {
        let this = Arc::new(Self {
            own_ip,
            routes: Mutex::new(Self::load()),
        });
        let watched = this.clone();
        tokio::spawn(async move {
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
        });
        this
    }
}

impl proxy::RouteResolver for FileRoutes {
    fn resolve(&self, host: &str) -> Option<proxy::Backend> {
        let routes = self.routes.lock().unwrap();
        // Exact matches win over a wildcard match — same precedence as
        // `daemon::WorkspaceRegistry::resolve_route`.
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
            })
    }
}

/// The sidecar's DNS-forwarder authority set is exactly its TLS/SNI-dispatch
/// route table — a name is "ours" the instant it's routable, no separate
/// zone-suffix bookkeeping needed. Reuses `RouteResolver::resolve` (already
/// polled/loaded) rather than a second data source.
impl dns::ZoneSource for FileRoutes {
    fn answer_for(&self, qname: &str) -> Option<Ipv4Addr> {
        proxy::RouteResolver::resolve(self, qname).map(|_| self.own_ip)
    }
}

/// Self-discovers this container's own address on the run's docker network
/// with no env var, no DNS lookup, and no dependency on anything else being
/// up yet: connecting a UDP socket doesn't send a packet, it only forces the
/// kernel to pick a local route/source address for the (unreachable, never
/// actually contacted) destination — reading it back off `local_addr()` is
/// this container's real in-network IP.
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
    let file_routes = FileRoutes::spawn_polling(own_ip);
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

    // Every node in this run points its `--dns` at this container first
    // (see `runs::start_node`) — recognized names (this run's
    // `fghj.internal` zone and any active alias) are answered directly with
    // this container's own address; anything else is forwarded verbatim to
    // Docker's embedded resolver, exactly like the host server forwards
    // nothing at all because the OS never routes it an out-of-zone query.
    let dns_socket = UdpSocket::bind(("0.0.0.0", 53))
        .await
        .context("failed to bind sidecar DNS listener on 0.0.0.0:53")?;
    let docker_embedded_dns = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 11), 53));
    println!("fghj-sidecar: DNS listening on 0.0.0.0:53, answering recognized names with {own_ip}");

    // No control API in this container — nothing in a run's network ever
    // presents the zone apex's own SNI, so this is never actually dialed.
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
    );
    Ok(())
}
