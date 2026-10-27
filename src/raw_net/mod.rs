//! Host-reachable raw ports for `fghj.raw.internal` nodes: a per-node virtual
//! IP NAT'd by the OS ([`crate::platform::RawNat`]) to the node's published
//! `127.0.0.1:<host_port>`.
//!
//! `virtual_ip_for` can collide; that only matters for two active names on the
//! same `(virtual_ip, port)`. `assign` keeps a sticky in-memory table that
//! moves a name only on a live conflict. Not persisted: the DNS answer and the
//! NAT rules only have to agree while the daemon is up.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::Ipv4Addr;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::platform;

/// Owned exclusively by fghjd, so alias pruning can claim the whole range.
const POOL_BASE: u32 = u32::from_be_bytes([10, 222, 0, 0]);
const POOL_SIZE: u32 = 1 << 16;

/// Pure, so DNS and NAT agree without coordinating. `10.222/16` rather than
/// Class E, which BSD stacks (macOS included) drop even when aliased.
pub fn virtual_ip_for(name: &str) -> Ipv4Addr {
    let digest = Sha256::digest(name.as_bytes());
    let hash = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    let offset = 1 + hash % (POOL_SIZE - 2);
    Ipv4Addr::from(POOL_BASE + offset)
}

fn slot_of(ip: Ipv4Addr) -> u32 {
    u32::from(ip) - POOL_BASE - 1
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEndpoint {
    pub raw_domain: String,
    pub ports: BTreeMap<String, u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct RouteSpec {
    pub virtual_ip: Ipv4Addr,
    pub container_port: u16,
    pub host_port: u16,
}

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

/// Keeps `preferred` unless a port collides with `claimed`, else probes
/// forward from the hash.
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
        .unwrap_or(naive)
}

/// A name keeps its slot unless it conflicts; names no longer active are
/// dropped.
fn assign(
    endpoints: &[RawEndpoint],
    previous: &HashMap<String, Ipv4Addr>,
) -> HashMap<String, Ipv4Addr> {
    let mut claimed: HashSet<(Ipv4Addr, u16)> = HashSet::new();
    let mut next: HashMap<String, Ipv4Addr> = HashMap::new();

    // Incumbents first, so a name moves only on a live conflict; both groups in
    // a fixed order, so results are deterministic.
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

fn assignments() -> &'static Mutex<HashMap<String, Ipv4Addr>> {
    static ASSIGNMENTS: OnceLock<Mutex<HashMap<String, Ipv4Addr>>> = OnceLock::new();
    ASSIGNMENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Falls back to the hash, so a name with no running container still gets an
/// answer.
pub fn resolve(name: &str) -> Ipv4Addr {
    assignments()
        .lock()
        .unwrap()
        .get(name)
        .copied()
        .unwrap_or_else(|| virtual_ip_for(name))
}

pub fn reconcile(endpoints: &[RawEndpoint]) -> Result<()> {
    let next = {
        let mut table = assignments().lock().unwrap();
        let next = assign(endpoints, &table);
        *table = next.clone();
        next
    };
    platform::current().nat.apply(&routes_for(endpoints, &next))
}

pub fn clear() -> Result<()> {
    assignments().lock().unwrap().clear();
    platform::current().nat.clear()
}

pub fn current_routes() -> Vec<RouteSpec> {
    platform::current().nat.status()
}

/// The applied routes, and those of them the OS isn't serving. `None` where
/// raw routing isn't implemented.
pub fn route_health() -> Option<Result<(Vec<RouteSpec>, Vec<RouteSpec>)>> {
    let routes = current_routes();
    let unserved = platform::current().nat.unserved(&routes)?;
    Some(unserved.map(|unserved| (routes, unserved)))
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

    fn endpoint(name: &str, port: u16) -> RawEndpoint {
        let mut ports = BTreeMap::new();
        ports.insert(port.to_string(), 1u16);
        RawEndpoint {
            raw_domain: name.to_string(),
            ports,
        }
    }

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
        let naive = virtual_ip_for(&a);
        assert!(assigned[&a] != naive || assigned[&b] != naive);
    }

    #[test]
    fn assign_does_not_collide_two_names_on_different_ports() {
        // pf rules key on (ip, port), so sharing an IP on different ports is fine.
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
