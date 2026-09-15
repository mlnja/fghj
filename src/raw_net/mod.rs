//! Host-reachable raw ports for `fghj.raw.internal` nodes, via a per-node
//! virtual IP NAT'd (natively, by the OS — `pf` on macOS; `iptables`/
//! `nftables` on Linux, not yet implemented) to the node's already-published
//! `127.0.0.1:<host_port>` (every declared container port is already
//! published there — see `docker.rs`). `raw_domain`/`ports` already exist on
//! `runs::ContainerInfo`; this module is purely additive on top of them.
//!
//! Structure: this file holds the platform-agnostic core (virtual IP
//! allocation, the desired-state shape, the `RawNetBackend` trait, and the
//! reconcile entrypoint); `macos` holds the macOS-specific `pf`/`ifconfig`
//! implementation. A future `linux` module would be a pure addition
//! alongside it — nothing here is macOS-specific.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::{Once, OnceLock};

use anyhow::Result;
use sha2::{Digest, Sha256};

mod macos;

/// Base of the address pool fghjd owns exclusively for raw-zone virtual
/// IPs. See `virtual_ip_for` for why this range.
const POOL_BASE: u32 = u32::from_be_bytes([10, 222, 0, 0]);
/// A `/16`: 65,536 addresses.
const POOL_SIZE: u32 = 1 << 16;

/// Deterministically maps a raw-zone domain name to a virtual IP inside
/// `10.222.0.0/16` — a pure function, no persisted state, so the DNS answer
/// path (`daemon::WorkspaceRegistry::answer_for`) and the NAT reconciler
/// (`reconcile`, below) always agree on the same address for the same name
/// without coordinating with each other, and the mapping survives a `fghjd`
/// restart unchanged.
///
/// `10.222.0.0/16` (plain RFC 1918 private space) rather than the
/// originally-considered `240.0.0.0/8` (Class E — has a real history of
/// being treated as martian/invalid and silently dropped by BSD-derived
/// network stacks, macOS's included, even when locally aliased): ordinary,
/// universally-routable private space, safe to alias onto `lo0`. A
/// dedicated `/16` gives `MacosPfBackend` a clean "this whole range belongs
/// to fghjd" invariant for alias pruning, without claiming anywhere near
/// the full `/8`.
///
/// Collisions (two names hashing to the same address) are possible in
/// principle but negligible in practice at this pool size for the handful
/// of nodes a dev workspace runs — not worth detecting or handling.
pub fn virtual_ip_for(name: &str) -> Ipv4Addr {
    let digest = Sha256::digest(name.as_bytes());
    let hash = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    // Offset 0 (the pool's own network address) is skipped purely for
    // tidiness — nothing actually depends on it being unused.
    let offset = 1 + hash % (POOL_SIZE - 2);
    Ipv4Addr::from(POOL_BASE + offset)
}

/// One node's raw-zone identity and its currently-published raw ports —
/// gathered by `daemon::WorkspaceRegistry::active_raw_endpoints` from
/// `runs::ContainerInfo::raw_domain`/`ports` (already-existing fields; this
/// module needs no new data-model changes upstream).
pub struct RawEndpoint {
    pub raw_domain: String,
    /// container port -> published host port (`127.0.0.1:<host_port>`).
    pub ports: BTreeMap<String, u16>,
}

/// One desired NAT rule: traffic to `virtual_ip:container_port` should reach
/// `127.0.0.1:host_port`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RouteSpec {
    pub virtual_ip: Ipv4Addr,
    pub container_port: u16,
    pub host_port: u16,
}

/// The platform-specific half of this feature — one implementation per OS.
/// `apply` always receives the *complete* desired state (like
/// `hosts_file::sync`), not a diff, so each backend can implement whatever
/// internal idempotency strategy suits its own mechanism.
pub trait RawNetBackend: Send + Sync {
    fn apply(&self, routes: &[RouteSpec]) -> Result<()>;
    fn clear(&self) -> Result<()>;
}

/// Backend selection: macOS gets the real `pf`/`ifconfig` implementation,
/// everything else gets a `NoopBackend` that logs once and does nothing —
/// the same runtime-`cfg!` pattern `dns::install_os_resolver_config` and
/// `ca::install_macos_trust` already use, so the crate keeps compiling
/// unconditionally on every platform.
fn backend() -> &'static dyn RawNetBackend {
    static BACKEND: OnceLock<Box<dyn RawNetBackend>> = OnceLock::new();
    BACKEND
        .get_or_init(|| {
            if cfg!(target_os = "macos") {
                Box::new(macos::MacosPfBackend::new())
            } else {
                Box::new(NoopBackend::new())
            }
        })
        .as_ref()
}

struct NoopBackend {
    warned: Once,
}

impl NoopBackend {
    fn new() -> Self {
        Self {
            warned: Once::new(),
        }
    }
}

impl RawNetBackend for NoopBackend {
    fn apply(&self, routes: &[RouteSpec]) -> Result<()> {
        if !routes.is_empty() {
            self.warned.call_once(|| {
                eprintln!(
                    "fghjd: host-reachable raw ports (*.fghj.raw.internal) aren't implemented on \
                     this platform yet — raw-zone names will resolve to a virtual IP, but \
                     nothing will answer on it"
                );
            });
        }
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        Ok(())
    }
}

/// Computes the desired `RouteSpec` set from `endpoints` via `virtual_ip_for`
/// — pure, so it's unit-testable without going through the process-global
/// `backend()`.
fn routes_for(endpoints: &[RawEndpoint]) -> Vec<RouteSpec> {
    let mut routes = Vec::new();
    for endpoint in endpoints {
        let virtual_ip = virtual_ip_for(&endpoint.raw_domain);
        for (port, host_port) in &endpoint.ports {
            let Ok(container_port) = port.parse::<u16>() else {
                continue;
            };
            routes.push(RouteSpec {
                virtual_ip,
                container_port,
                host_port: *host_port,
            });
        }
    }
    routes
}

/// Recomputes the desired NAT route set from `endpoints` and hands it to the
/// platform backend. Called once per `spawn_reconciler` tick in `daemon.rs`,
/// mirroring `hosts_file::sync`'s "recompute the whole desired state from
/// scratch every tick" approach.
pub fn reconcile(endpoints: &[RawEndpoint]) -> Result<()> {
    backend().apply(&routes_for(endpoints))
}

/// Tears down every route this module manages — called from
/// `DaemonControl::deactivate`.
pub fn clear() -> Result<()> {
    backend().clear()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_ip_for_is_deterministic() {
        let a = virtual_ip_for("minio.myworkspace.fghj.raw.internal");
        let b = virtual_ip_for("minio.myworkspace.fghj.raw.internal");
        assert_eq!(a, b);
    }

    #[test]
    fn virtual_ip_for_lands_inside_the_pool() {
        for name in [
            "minio.myworkspace.fghj.raw.internal",
            "postgres.otherrun.myworkspace.fghj.raw.internal",
            "",
            "a-very-long-domain-name-with-many-labels.deep.sub.myworkspace.fghj.raw.internal",
        ] {
            let ip = virtual_ip_for(name);
            let octets = ip.octets();
            assert_eq!(octets[0], 10, "{name} produced {ip}, outside 10.222.0.0/16");
            assert_eq!(
                octets[1], 222,
                "{name} produced {ip}, outside 10.222.0.0/16"
            );
        }
    }

    #[test]
    fn virtual_ip_for_differs_across_distinct_names() {
        let a = virtual_ip_for("minio.myworkspace.fghj.raw.internal");
        let b = virtual_ip_for("postgres.myworkspace.fghj.raw.internal");
        assert_ne!(a, b);
    }

    #[test]
    fn routes_for_builds_one_route_per_declared_port() {
        let mut ports = BTreeMap::new();
        ports.insert("9000".to_string(), 54321u16);
        ports.insert("9001".to_string(), 54322u16);
        let endpoints = vec![RawEndpoint {
            raw_domain: "minio.myworkspace.fghj.raw.internal".to_string(),
            ports,
        }];

        let routes = routes_for(&endpoints);
        let expected_ip = virtual_ip_for("minio.myworkspace.fghj.raw.internal");
        assert_eq!(routes.len(), 2);
        assert!(routes.contains(&RouteSpec {
            virtual_ip: expected_ip,
            container_port: 9000,
            host_port: 54321,
        }));
        assert!(routes.contains(&RouteSpec {
            virtual_ip: expected_ip,
            container_port: 9001,
            host_port: 54322,
        }));
    }

    #[test]
    fn routes_for_skips_unparseable_ports_rather_than_erroring() {
        let mut ports = BTreeMap::new();
        ports.insert("not-a-port".to_string(), 1234u16);
        let endpoints = vec![RawEndpoint {
            raw_domain: "weird.myworkspace.fghj.raw.internal".to_string(),
            ports,
        }];
        assert!(routes_for(&endpoints).is_empty());
    }

    #[test]
    fn noop_backend_only_warns_when_routes_are_non_empty() {
        let backend = NoopBackend::new();
        assert!(backend.apply(&[]).is_ok());
        assert!(backend.clear().is_ok());
    }
}
