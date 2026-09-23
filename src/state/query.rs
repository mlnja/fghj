//! Read-only projections over every registered workspace's state.
//!
//! Four consumers each used to walk their own copy of the same "every
//! running container in every workspace" loop: `WorkspaceRegistry`'s
//! `resolve_route` (SNI -> backend port), `active_wildcard_suffixes` (DNS
//! zones) and `active_raw_endpoints` (raw-net telemetry) walked the *old*
//! `runs::RunRegistry`, while `effects::dns` and `effects::raw_net` walked
//! the new `state::WorkspaceState` for the same facts. This is that walk,
//! written once, over the new state only.
//!
//! Everything here is a pure function of a `BTreeMap<workspace id,
//! WorkspaceState>` — the same shape `FannedInEffect::extract` receives, and
//! the shape `registry::ActorRegistry::states` hands out.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::raw_net::RawEndpoint;

use super::{ContainerInfo, PortRoute, WorkspaceState};

/// Every container Docker last reported as `"running"`, across every
/// workspace. The filter each projection below shares: a stopped — or
/// stopped-but-not-yet-reconciled — container must never hand back a route,
/// a DNS zone, or a NAT rule pointing at a port nothing is listening on.
pub fn running_containers(
    states: &BTreeMap<String, Arc<WorkspaceState>>,
) -> impl Iterator<Item = &ContainerInfo> {
    states
        .values()
        .flat_map(|state| state.runs.values())
        .flat_map(|run| run.containers.values())
        .filter(|container| container.observed.status == "running")
}

/// Where `route` actually points right now: the host port Docker last
/// reported for the container-side port this route was derived from, not
/// the one recorded when the route was first computed. The two differ
/// whenever Docker republishes a container on a fresh ephemeral port — a
/// restart-policy-triggered restart, or `dockerd` itself restarting — which
/// `desired.routes` has no way to learn about on its own, since nothing
/// fghj did caused it.
///
/// Falls back to the route's own recorded port when the container port
/// isn't in `observed.ports`: a route persisted before `container_port`
/// existed has no key to look up, and a container nothing has re-observed
/// yet has nothing better to offer.
fn live_host_port(container: &ContainerInfo, route: &PortRoute) -> u16 {
    container
        .observed
        .ports
        .get(&route.container_port)
        .copied()
        .flatten()
        .unwrap_or(route.host_port)
}

/// The `127.0.0.1:<port>` a running container publishes `host` at, if any —
/// the SNI -> container lookup backing per-service HTTPS routing (see
/// `proxy::serve_https`).
///
/// Exact matches (a node's own derived domain, a named port, or a literal
/// `#AdditionalHost`) always win over a wildcard match: a `wildcard_hosts`
/// suffix only ever fills in for a name nothing more specific already
/// claims. That's why this is two passes over every workspace rather than
/// one pass preferring exact matches within each — a wildcard in the first
/// workspace looked at must still lose to an exact match in the last.
pub fn resolve_route(states: &BTreeMap<String, Arc<WorkspaceState>>, host: &str) -> Option<u16> {
    let exact = running_containers(states).find_map(|container| {
        container
            .desired
            .routes
            .iter()
            .find(|route| !route.wildcard && route.domain == host)
            .map(|route| live_host_port(container, route))
    });

    exact.or_else(|| {
        running_containers(states).find_map(|container| {
            container
                .desired
                .routes
                .iter()
                .find(|route| route.wildcard && matches_suffix(host, &route.domain))
                .map(|route| live_host_port(container, route))
        })
    })
}

/// Whether `host` is covered by the wildcard route `suffix` — the suffix
/// itself, or any name under it. Deliberately not a substring check:
/// `notmyservice.local` must not match `myservice.local`.
fn matches_suffix(host: &str, suffix: &str) -> bool {
    host == suffix || host.ends_with(&format!(".{suffix}"))
}

/// Every `wildcard_hosts` suffix currently claimed by a running container,
/// sorted and deduplicated — both the zones `dns::install_os_resolver_config`
/// points at fghj's DNS server, and the zones that server answers for.
/// Sorted so an unchanged set compares equal as a `FannedInEffect::Snapshot`
/// regardless of which workspace contributed what.
pub fn wildcard_suffixes(states: &BTreeMap<String, Arc<WorkspaceState>>) -> Vec<String> {
    let mut zones: Vec<String> = running_containers(states)
        .flat_map(|container| {
            container
                .desired
                .routes
                .iter()
                .filter(|route| route.wildcard)
                .map(|route| route.domain.clone())
        })
        .collect();
    zones.sort();
    zones.dedup();
    zones
}

/// Every running container's raw-zone identity and the host ports Docker
/// currently publishes it on — the input to `raw_net::reconcile`'s
/// virtual-IP NAT, and to the telemetry drawer's network tab. Unpublished
/// ports (`None`) are dropped rather than carried as a hole: there's no
/// address to NAT to.
pub fn raw_endpoints(states: &BTreeMap<String, Arc<WorkspaceState>>) -> Vec<RawEndpoint> {
    running_containers(states)
        .map(|container| RawEndpoint {
            raw_domain: container.desired.raw_domain.clone(),
            ports: container
                .observed
                .ports
                .iter()
                .filter_map(|(port, host_port)| host_port.map(|p| (port.clone(), p)))
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ContainerDesired, ContainerObserved, RunState};

    fn route(domain: &str, host_port: u16, wildcard: bool, container_port: &str) -> PortRoute {
        PortRoute {
            domain: domain.to_string(),
            host_port,
            wildcard,
            container_port: container_port.to_string(),
            https: true,
        }
    }

    fn container(node_id: &str, status: &str, routes: Vec<PortRoute>) -> ContainerInfo {
        ContainerInfo {
            node_id: node_id.to_string(),
            desired: ContainerDesired {
                running: status == "running",
                container_name: format!("fghj-{node_id}-1"),
                domain: format!("{node_id}.fghj.internal"),
                raw_domain: format!("{node_id}.fghj.raw.internal"),
                routes,
                additional_hosts: vec![],
                status_port: None,
                config_hash: "hash".into(),
            },
            observed: ContainerObserved {
                status: status.to_string(),
                ..Default::default()
            },
            pending_action: None,
        }
    }

    /// `id -> WorkspaceState` holding a single `default` run, built from
    /// `(workspace id, containers)` pairs.
    fn states(
        workspaces: Vec<(&str, Vec<ContainerInfo>)>,
    ) -> BTreeMap<String, Arc<WorkspaceState>> {
        workspaces
            .into_iter()
            .map(|(id, containers)| {
                let run = RunState {
                    run_id: "default".into(),
                    network: "fghj-net".into(),
                    containers: containers
                        .into_iter()
                        .map(|c| (c.node_id.clone(), c))
                        .collect(),
                    volumes: BTreeMap::new(),
                    sidecar_container_name: "fghj-sidecar".into(),
                    sidecar_ip: None,
                    pending_create: None,
                };
                let state = WorkspaceState {
                    runs: BTreeMap::from([("default".to_string(), run)]),
                    ..Default::default()
                };
                (id.to_string(), Arc::new(state))
            })
            .collect()
    }

    #[test]
    fn resolve_route_finds_an_exact_match() {
        let states = states(vec![(
            "ws",
            vec![container(
                "web",
                "running",
                vec![route("web.fghj.internal", 5001, false, "80")],
            )],
        )]);
        assert_eq!(resolve_route(&states, "web.fghj.internal"), Some(5001));
        assert_eq!(resolve_route(&states, "other.fghj.internal"), None);
    }

    #[test]
    fn resolve_route_ignores_containers_that_are_not_running() {
        let states = states(vec![(
            "ws",
            vec![container(
                "web",
                "exited",
                vec![route("web.fghj.internal", 5001, false, "80")],
            )],
        )]);
        assert_eq!(resolve_route(&states, "web.fghj.internal"), None);
    }

    #[test]
    fn resolve_route_falls_back_to_a_wildcard_only_for_unclaimed_names() {
        let states = states(vec![(
            "ws",
            vec![
                container(
                    "web",
                    "running",
                    vec![route("app.example.com", 5001, false, "80")],
                ),
                container(
                    "catchall",
                    "running",
                    vec![route("example.com", 5002, true, "80")],
                ),
            ],
        )]);
        // The exact route wins for the name it claims...
        assert_eq!(resolve_route(&states, "app.example.com"), Some(5001));
        // ...and the wildcard covers the suffix itself and anything else
        // under it.
        assert_eq!(resolve_route(&states, "example.com"), Some(5002));
        assert_eq!(resolve_route(&states, "other.example.com"), Some(5002));
        // A name that merely ends with the same characters is not under it.
        assert_eq!(resolve_route(&states, "notexample.com"), None);
    }

    /// The exact-beats-wildcard rule has to hold across workspaces, not
    /// just within one — a wildcard in the workspace that happens to sort
    /// first must still lose to an exact match in a later one.
    #[test]
    fn resolve_route_prefers_an_exact_match_in_a_later_workspace_over_an_earlier_wildcard() {
        let states = states(vec![
            (
                "ws-a",
                vec![container(
                    "catchall",
                    "running",
                    vec![route("example.com", 5002, true, "80")],
                )],
            ),
            (
                "ws-b",
                vec![container(
                    "web",
                    "running",
                    vec![route("app.example.com", 5001, false, "80")],
                )],
            ),
        ]);
        assert_eq!(resolve_route(&states, "app.example.com"), Some(5001));
    }

    /// The drift `refresh` used to correct by rewriting `desired.routes` in
    /// place: Docker republished the container on a different host port, so
    /// the route's own recorded port is stale and `observed.ports` is the
    /// only current truth.
    #[test]
    fn resolve_route_prefers_the_freshly_observed_port_over_the_recorded_one() {
        let mut c = container(
            "web",
            "running",
            vec![route("web.fghj.internal", 5001, false, "80")],
        );
        c.observed.ports = BTreeMap::from([("80".to_string(), Some(6002))]);
        let states = states(vec![("ws", vec![c])]);
        assert_eq!(resolve_route(&states, "web.fghj.internal"), Some(6002));
    }

    #[test]
    fn resolve_route_keeps_the_recorded_port_when_nothing_has_observed_that_container_port() {
        let states = states(vec![(
            "ws",
            vec![container(
                "web",
                "running",
                vec![route("web.fghj.internal", 5001, false, "80")],
            )],
        )]);
        assert_eq!(resolve_route(&states, "web.fghj.internal"), Some(5001));
    }

    #[test]
    fn wildcard_suffixes_are_sorted_deduplicated_and_running_only() {
        let mut stopped = container("old", "exited", vec![route("gone.local", 5003, true, "80")]);
        stopped.observed.status = "exited".into();
        let states = states(vec![
            (
                "ws-a",
                vec![container(
                    "web",
                    "running",
                    vec![route("b.local", 5001, true, "80")],
                )],
            ),
            (
                "ws-b",
                vec![
                    container(
                        "api",
                        "running",
                        vec![
                            route("a.local", 5002, true, "80"),
                            route("b.local", 5002, true, "81"),
                            route("exact.local", 5002, false, "82"),
                        ],
                    ),
                    stopped,
                ],
            ),
        ]);
        assert_eq!(wildcard_suffixes(&states), vec!["a.local", "b.local"]);
    }

    #[test]
    fn raw_endpoints_carry_published_ports_only() {
        let mut c = container("web", "running", vec![]);
        c.observed.ports =
            BTreeMap::from([("80".to_string(), Some(6002)), ("9000".to_string(), None)]);
        let states = states(vec![("ws", vec![c])]);
        let endpoints = raw_endpoints(&states);
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].raw_domain, "web.fghj.raw.internal");
        assert_eq!(
            endpoints[0].ports,
            BTreeMap::from([("80".to_string(), 6002)])
        );
    }

    /// A plain TCP dependency with nothing published yet still needs its
    /// raw-zone identity to exist, so its virtual IP resolves rather than
    /// NXDOMAIN-ing while it comes up.
    #[test]
    fn raw_endpoints_include_a_running_container_with_no_published_ports() {
        let states = states(vec![("ws", vec![container("web", "running", vec![])])]);
        let endpoints = raw_endpoints(&states);
        assert_eq!(endpoints.len(), 1);
        assert!(endpoints[0].ports.is_empty());
    }

    #[test]
    fn raw_endpoints_aggregate_across_workspaces_and_exclude_stopped_containers() {
        let states = states(vec![
            ("ws-a", vec![container("web", "running", vec![])]),
            (
                "ws-b",
                vec![
                    container("api", "running", vec![]),
                    container("old", "exited", vec![]),
                ],
            ),
        ]);
        assert_eq!(raw_endpoints(&states).len(), 2);
    }

    #[test]
    fn projections_over_no_workspaces_are_empty() {
        let none = BTreeMap::new();
        assert_eq!(resolve_route(&none, "web.fghj.internal"), None);
        assert!(wildcard_suffixes(&none).is_empty());
        assert!(raw_endpoints(&none).is_empty());
    }
}
