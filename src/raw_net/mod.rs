//! Host-reachable raw ports for `fghj.raw.internal` nodes, via a per-node
//! virtual IP NAT'd (natively, by the OS — `pf` on macOS; `iptables`/
//! `nftables` on Linux, not yet implemented) to the node's already-published
//! `127.0.0.1:<host_port>` (every declared container port is already
//! published there — see `docker.rs`). `raw_domain`/`ports` already exist on
//! `state::ContainerInfo`; this module is purely additive on top of them.
//!
//! Structure: this file holds the platform-agnostic core (virtual IP
//! allocation, the desired-state shape, the `RawNetBackend` trait, and the
//! reconcile entrypoint); `macos` holds the macOS-specific `pf`/`ifconfig`
//! implementation. A future `linux` module would be a pure addition
//! alongside it — nothing here is macOS-specific.
//!
//! `virtual_ip_for` is a pure hash and can collide (negligible odds at
//! dev-workspace scale, but non-zero — see its doc comment). A collision is
//! only a real problem when it lands two *active* names on the same
//! `(virtual_ip, container_port)` pair, since that's what a `pf` rule keys
//! on. `assign`/`resolve` add a thin, sticky, in-memory layer on top of the
//! naive hash to fix that: `reconcile` keeps each active name's existing
//! assignment as long as it doesn't conflict with another active name's
//! claim, and only (re)computes a name's slot when it's newly active or its
//! prior slot now genuinely conflicts. This is deliberately *not* persisted
//! to disk — a restart simply recomputes it from whatever's active at that
//! moment, which is fine, since nothing about correctness depends on
//! matching a previous run's specific addresses, only on the DNS answer
//! path (`resolve`) and the NAT reconciler (`reconcile`) agreeing with each
//! other while the daemon is up.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::Ipv4Addr;
use std::sync::{Mutex, Once, OnceLock};

use anyhow::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::daemon_log;

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

/// Inverse of `virtual_ip_for`'s offset step — recovers the `0..POOL_SIZE-2`
/// slot number an address occupies, so `pick_ip` can probe forward from it.
fn slot_of(ip: Ipv4Addr) -> u32 {
    u32::from(ip) - POOL_BASE - 1
}

/// One node's raw-zone identity and its currently-published raw ports —
/// projected out of `state::ContainerInfo`'s `desired.raw_domain` and
/// `observed.ports` by `state::query::raw_endpoints`. Comparable and
/// cloneable so `effects::raw_net` can use it directly as its
/// `FannedInEffect::Snapshot` (the "did anything actually change" check)
/// rather than keeping a parallel struct of the same three fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEndpoint {
    pub raw_domain: String,
    /// container port -> published host port (`127.0.0.1:<host_port>`).
    pub ports: BTreeMap<String, u16>,
}

/// One desired NAT rule: traffic to `virtual_ip:container_port` should reach
/// `127.0.0.1:host_port`. `Serialize`d as-is for the telemetry drawer's
/// network-status tab (`daemon.rs`'s `/daemon/net-status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
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
    /// The route set currently installed, per the backend's own last-applied
    /// record — not recomputed from `RawEndpoint`s, so it reflects reality
    /// even if a route was applied by an earlier `fghjd` process (or
    /// couldn't be, on a backend that doesn't support this platform).
    fn status(&self) -> Vec<RouteSpec>;
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
                daemon_log::warn(
                    "fghjd: host-reachable raw ports (*.fghj.raw.internal) aren't implemented on \
                     this platform yet — raw-zone names will resolve to a virtual IP, but \
                     nothing will answer on it"
                        .to_string(),
                );
            });
        }
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        Ok(())
    }

    fn status(&self) -> Vec<RouteSpec> {
        Vec::new()
    }
}

/// Computes the desired `RouteSpec` set from `endpoints`, using `assigned`
/// (an up-to-date sticky table from `assign`) in preference to the naive
/// hash — pure, so it's unit-testable without going through the
/// process-global `backend()`.
fn routes_for(endpoints: &[RawEndpoint], assigned: &HashMap<String, Ipv4Addr>) -> Vec<RouteSpec> {
    let mut routes = Vec::new();
    for endpoint in endpoints {
        let virtual_ip = assigned
            .get(&endpoint.raw_domain)
            .copied()
            .unwrap_or_else(|| virtual_ip_for(&endpoint.raw_domain));
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

/// Picks a virtual IP for a name given its declared ports and the
/// `(virtual_ip, port)` pairs already claimed by other active names this
/// round: keeps `preferred` (its existing sticky assignment, if any) as long
/// as none of `ports` collide with `claimed`, otherwise starts from the
/// plain hash and probes forward (wrapping within the pool) until every
/// port is free. Pure, so it's testable without real hash collisions —
/// callers can hand it synthetic `claimed` sets directly.
fn pick_ip(
    preferred: Option<Ipv4Addr>,
    naive: Ipv4Addr,
    ports: &[u16],
    claimed: &HashSet<(Ipv4Addr, u16)>,
) -> Ipv4Addr {
    let free_at = |ip: Ipv4Addr| ports.iter().all(|p| !claimed.contains(&(ip, *p)));

    if let Some(ip) = preferred
        && free_at(ip)
    {
        return ip;
    }

    let span = POOL_SIZE - 2;
    let start = slot_of(naive);
    (0..span)
        .map(|step| Ipv4Addr::from(POOL_BASE + 1 + (start + step) % span))
        .find(|ip| free_at(*ip))
        // Pool exhausted — never happens at dev-workspace scale — fall back
        // to the naive address rather than panicking.
        .unwrap_or(naive)
}

/// Recomputes the sticky name -> virtual-IP table from the currently-active
/// `endpoints` and whatever was assigned last round (`previous`). A name
/// keeps its previous slot as long as it doesn't conflict with another
/// active name's claim on the same `(virtual_ip, container_port)` pair;
/// only names that are newly active, or whose prior slot now genuinely
/// conflicts, get (re)computed. A name absent from `endpoints` is simply
/// dropped — its slot isn't reserved and doesn't affect anyone else's.
/// Pure (no process-global state), so it's unit-testable directly; see
/// `reconcile` for the stateful wrapper that feeds it from/to `assignments`.
fn assign(
    endpoints: &[RawEndpoint],
    previous: &HashMap<String, Ipv4Addr>,
) -> HashMap<String, Ipv4Addr> {
    let mut claimed: HashSet<(Ipv4Addr, u16)> = HashSet::new();
    let mut next: HashMap<String, Ipv4Addr> = HashMap::new();

    // Incumbents (already assigned) go first, in a fixed order, so a name
    // only ever moves because of a live conflict — never merely because a
    // "better" slot happened to free up elsewhere. Newcomers are resolved
    // afterwards, also in a fixed order, so simultaneous new arrivals that
    // collide with each other still resolve deterministically.
    let mut incumbents: Vec<&RawEndpoint> = Vec::new();
    let mut newcomers: Vec<&RawEndpoint> = Vec::new();
    for endpoint in endpoints {
        if previous.contains_key(&endpoint.raw_domain) {
            incumbents.push(endpoint);
        } else {
            newcomers.push(endpoint);
        }
    }
    incumbents.sort_by(|a, b| a.raw_domain.cmp(&b.raw_domain));
    newcomers.sort_by(|a, b| a.raw_domain.cmp(&b.raw_domain));

    for endpoint in incumbents.into_iter().chain(newcomers) {
        let ports: Vec<u16> = endpoint
            .ports
            .keys()
            .filter_map(|p| p.parse::<u16>().ok())
            .collect();
        let naive = virtual_ip_for(&endpoint.raw_domain);
        let preferred = previous.get(&endpoint.raw_domain).copied();
        let ip = pick_ip(preferred, naive, &ports, &claimed);

        for port in &ports {
            claimed.insert((ip, *port));
        }
        next.insert(endpoint.raw_domain.clone(), ip);
    }

    next
}

/// The sticky assignment table `assign` maintains across `reconcile` ticks —
/// in-memory only, deliberately not persisted (see the module doc comment).
fn assignments() -> &'static Mutex<HashMap<String, Ipv4Addr>> {
    static ASSIGNMENTS: OnceLock<Mutex<HashMap<String, Ipv4Addr>>> = OnceLock::new();
    ASSIGNMENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolves a raw-zone name to its virtual IP: the sticky assignment if the
/// name is (or was, as of the last reconcile tick) active, else the plain
/// hash — used by `daemon::WorkspaceRegistry::answer_for` so that a name
/// with no running backing container still gets an answer (consistent with
/// today's "always answer in-zone" behavior), just one nothing NATs to.
pub fn resolve(name: &str) -> Ipv4Addr {
    assignments()
        .lock()
        .unwrap()
        .get(name)
        .copied()
        .unwrap_or_else(|| virtual_ip_for(name))
}

/// Recomputes the sticky assignment table and the desired NAT route set from
/// `endpoints`, then hands the routes to the platform backend. Called once
/// per `spawn_reconciler` tick in `daemon.rs`, mirroring `hosts_file::sync`'s
/// "recompute the whole desired state from scratch every tick" approach —
/// except for the assignment table itself, which is intentionally sticky
/// (see `assign`).
pub fn reconcile(endpoints: &[RawEndpoint]) -> Result<()> {
    let next = {
        let mut table = assignments().lock().unwrap();
        let next = assign(endpoints, &table);
        *table = next.clone();
        next
    };
    backend().apply(&routes_for(endpoints, &next))
}

/// Tears down every route this module manages and forgets every sticky
/// assignment — called from `DaemonControl::deactivate`.
pub fn clear() -> Result<()> {
    assignments().lock().unwrap().clear();
    backend().clear()
}

/// The route set currently installed — backs the telemetry drawer's
/// network-status tab (`daemon.rs`'s `/daemon/net-status`).
pub fn current_routes() -> Vec<RouteSpec> {
    backend().status()
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

        let expected_ip = virtual_ip_for("minio.myworkspace.fghj.raw.internal");
        let mut assigned = HashMap::new();
        assigned.insert(
            "minio.myworkspace.fghj.raw.internal".to_string(),
            expected_ip,
        );
        let routes = routes_for(&endpoints, &assigned);
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
    fn routes_for_falls_back_to_the_naive_hash_when_unassigned() {
        let mut ports = BTreeMap::new();
        ports.insert("9000".to_string(), 54321u16);
        let endpoints = vec![RawEndpoint {
            raw_domain: "minio.myworkspace.fghj.raw.internal".to_string(),
            ports,
        }];
        let routes = routes_for(&endpoints, &HashMap::new());
        assert_eq!(
            routes[0].virtual_ip,
            virtual_ip_for("minio.myworkspace.fghj.raw.internal")
        );
    }

    #[test]
    fn routes_for_skips_unparseable_ports_rather_than_erroring() {
        let mut ports = BTreeMap::new();
        ports.insert("not-a-port".to_string(), 1234u16);
        let endpoints = vec![RawEndpoint {
            raw_domain: "weird.myworkspace.fghj.raw.internal".to_string(),
            ports,
        }];
        assert!(routes_for(&endpoints, &HashMap::new()).is_empty());
    }

    #[test]
    fn noop_backend_only_warns_when_routes_are_non_empty() {
        let backend = NoopBackend::new();
        assert!(backend.apply(&[]).is_ok());
        assert!(backend.clear().is_ok());
        assert!(backend.status().is_empty());
    }

    fn endpoint(name: &str, port: u16) -> RawEndpoint {
        let mut ports = BTreeMap::new();
        ports.insert(port.to_string(), 1u16);
        RawEndpoint {
            raw_domain: name.to_string(),
            ports,
        }
    }

    /// Brute-forces a pair of distinct names that collide under
    /// `virtual_ip_for` — expected within a few hundred draws given the pool
    /// size, so this stays fast and needs no hardcoded magic strings.
    fn find_colliding_pair() -> (String, String) {
        let mut seen: HashMap<Ipv4Addr, String> = HashMap::new();
        for i in 0.. {
            let name = format!("svc-{i}.myworkspace.fghj.raw.internal");
            let ip = virtual_ip_for(&name);
            if let Some(other) = seen.get(&ip) {
                return (other.clone(), name);
            }
            seen.insert(ip, name);
        }
        unreachable!()
    }

    #[test]
    fn pick_ip_keeps_the_preferred_slot_when_free() {
        let preferred = Ipv4Addr::new(10, 222, 1, 1);
        let naive = Ipv4Addr::new(10, 222, 2, 2);
        let ip = pick_ip(Some(preferred), naive, &[80], &HashSet::new());
        assert_eq!(ip, preferred);
    }

    #[test]
    fn pick_ip_probes_forward_when_the_preferred_slot_conflicts() {
        let preferred = Ipv4Addr::new(10, 222, 1, 1);
        let naive = Ipv4Addr::new(10, 222, 1, 1);
        let mut claimed = HashSet::new();
        claimed.insert((preferred, 80));
        let ip = pick_ip(Some(preferred), naive, &[80], &claimed);
        assert_ne!(ip, preferred);
        assert!(!claimed.contains(&(ip, 80)));
    }

    #[test]
    fn pick_ip_ignores_conflicts_on_ports_it_does_not_use() {
        let preferred = Ipv4Addr::new(10, 222, 1, 1);
        let mut claimed = HashSet::new();
        claimed.insert((preferred, 443)); // different port, same address
        let ip = pick_ip(Some(preferred), preferred, &[80], &claimed);
        assert_eq!(ip, preferred);
    }

    #[test]
    fn assign_gives_new_uncontested_names_their_naive_hash() {
        let endpoints = vec![endpoint("solo.myworkspace.fghj.raw.internal", 80)];
        let assigned = assign(&endpoints, &HashMap::new());
        assert_eq!(
            assigned["solo.myworkspace.fghj.raw.internal"],
            virtual_ip_for("solo.myworkspace.fghj.raw.internal")
        );
    }

    #[test]
    fn assign_drops_entries_for_names_no_longer_active() {
        let mut previous = HashMap::new();
        previous.insert(
            "gone.myworkspace.fghj.raw.internal".to_string(),
            Ipv4Addr::new(10, 222, 9, 9),
        );
        assert!(assign(&[], &previous).is_empty());
    }

    #[test]
    fn assign_resolves_a_fresh_collision_by_moving_one_name() {
        let (a, b) = find_colliding_pair();
        let endpoints = vec![endpoint(&a, 80), endpoint(&b, 80)];

        let assigned = assign(&endpoints, &HashMap::new());
        assert_ne!(
            assigned[&a], assigned[&b],
            "colliding names sharing a port must end up on different virtual IPs"
        );
        // At least one of the two had to move off the shared naive slot.
        let naive = virtual_ip_for(&a);
        assert!(assigned[&a] != naive || assigned[&b] != naive);
    }

    #[test]
    fn assign_does_not_collide_two_names_on_different_ports() {
        // Same collision pair, but declared on different ports — a real pf
        // rule is keyed on (ip, port), so sharing an IP here is harmless and
        // shouldn't trigger a reassignment.
        let (a, b) = find_colliding_pair();
        let endpoints = vec![endpoint(&a, 80), endpoint(&b, 81)];
        let assigned = assign(&endpoints, &HashMap::new());
        assert_eq!(assigned[&a], virtual_ip_for(&a));
        assert_eq!(assigned[&b], virtual_ip_for(&b));
    }

    #[test]
    fn assign_keeps_an_incumbent_when_its_collision_partner_disappears() {
        let (a, b) = find_colliding_pair();
        let endpoints = vec![endpoint(&a, 80), endpoint(&b, 80)];
        let first_round = assign(&endpoints, &HashMap::new());

        // `a` stops; only `b` remains, carrying its `first_round` slot in as
        // `previous`. Removing its collision partner should not move it.
        let remaining = vec![endpoint(&b, 80)];
        let second_round = assign(&remaining, &first_round);

        assert_eq!(
            second_round[&b], first_round[&b],
            "removing an unrelated collision partner shouldn't reassign the survivor"
        );
    }

    #[test]
    fn resolve_falls_back_to_the_naive_hash_for_unknown_names() {
        let name = "unknown-to-resolve-test.fghj.raw.internal";
        assert_eq!(resolve(name), virtual_ip_for(name));
    }
}
