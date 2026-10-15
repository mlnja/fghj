//! Read-only projections over every registered workspace's state.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::raw_net::RawEndpoint;

use super::{ContainerInfo, PortRoute, WorkspaceState};

/// Every container Docker last reported as running, in every workspace. A
/// stopped one must never get a route, zone or NAT rule.
pub fn running_containers(
    states: &BTreeMap<String, Arc<WorkspaceState>>,
) -> impl Iterator<Item = &ContainerInfo> {
    states
        .values()
        .flat_map(|state| state.env.iter())
        .flat_map(|run| run.containers.values())
        .filter(|container| container.observed.status == "running")
}

/// The host port Docker currently publishes the route's container port on,
/// which changes when Docker republishes; falls back to the recorded one.
fn live_host_port(container: &ContainerInfo, route: &PortRoute) -> u16 {
    container
        .observed
        .ports
        .get(&route.container_port)
        .copied()
        .flatten()
        .unwrap_or(route.host_port)
}

/// Which node a routed name belongs to, and where it listens on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTarget {
    pub workspace: String,
    pub node_id: String,
    pub port: u16,
}

/// Exact matches beat wildcards across all workspaces, hence two passes.
pub fn resolve_route(
    states: &BTreeMap<String, Arc<WorkspaceState>>,
    host: &str,
) -> Option<RouteTarget> {
    let find = |matches: &dyn Fn(&PortRoute) -> bool| {
        states.iter().find_map(|(workspace, state)| {
            state
                .env
                .iter()
                .flat_map(|run| run.containers.values())
                .filter(|container| container.observed.status == "running")
                .find_map(|container| {
                    let route = container.desired.routes.iter().find(|r| matches(r))?;
                    Some(RouteTarget {
                        workspace: workspace.clone(),
                        node_id: container.node_id.clone(),
                        port: live_host_port(container, route),
                    })
                })
        })
    };
    find(&|route| !route.wildcard && route.domain == host)
        .or_else(|| find(&|route| route.wildcard && matches_suffix(host, &route.domain)))
}

/// Not a substring check: `notmyservice.local` doesn't match `myservice.local`.
fn matches_suffix(host: &str, suffix: &str) -> bool {
    host == suffix || host.ends_with(&format!(".{suffix}"))
}

/// Sorted and deduped so the snapshot compares equal when unchanged.
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

/// Unpublished ports are dropped: nothing to NAT to.
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
    use crate::state::testing;

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
        let mut c = testing::container(node_id);
        c.desired.running = status == "running";
        c.desired.routes = routes;
        c.observed.status = status.to_string();
        c
    }

    fn states(
        workspaces: Vec<(&str, Vec<ContainerInfo>)>,
    ) -> BTreeMap<String, Arc<WorkspaceState>> {
        workspaces
            .into_iter()
            .map(|(id, containers)| {
                let state = testing::workspace(containers);
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
        assert_eq!(
            resolve_route(&states, "web.fghj.internal").map(|t| t.port),
            Some(5001)
        );
        assert_eq!(
            resolve_route(&states, "other.fghj.internal").map(|t| t.port),
            None
        );
    }

    #[test]
    fn resolve_route_names_the_workspace_and_node_it_found() {
        let states = states(vec![
            ("a", vec![container("db", "running", vec![])]),
            (
                "b",
                vec![container(
                    "web",
                    "running",
                    vec![route("web.fghj.internal", 5001, false, "80")],
                )],
            ),
        ]);
        assert_eq!(
            resolve_route(&states, "web.fghj.internal"),
            Some(RouteTarget {
                workspace: "b".to_string(),
                node_id: "web".to_string(),
                port: 5001,
            })
        );
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
        assert_eq!(
            resolve_route(&states, "web.fghj.internal").map(|t| t.port),
            None
        );
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
        assert_eq!(
            resolve_route(&states, "app.example.com").map(|t| t.port),
            Some(5001)
        );
        assert_eq!(
            resolve_route(&states, "example.com").map(|t| t.port),
            Some(5002)
        );
        assert_eq!(
            resolve_route(&states, "other.example.com").map(|t| t.port),
            Some(5002)
        );
        assert_eq!(
            resolve_route(&states, "notexample.com").map(|t| t.port),
            None
        );
    }

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
        assert_eq!(
            resolve_route(&states, "app.example.com").map(|t| t.port),
            Some(5001)
        );
    }

    #[test]
    fn resolve_route_prefers_the_freshly_observed_port_over_the_recorded_one() {
        let mut c = container(
            "web",
            "running",
            vec![route("web.fghj.internal", 5001, false, "80")],
        );
        c.observed.ports = BTreeMap::from([("80".to_string(), Some(6002))]);
        let states = states(vec![("ws", vec![c])]);
        assert_eq!(
            resolve_route(&states, "web.fghj.internal").map(|t| t.port),
            Some(6002)
        );
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
        assert_eq!(
            resolve_route(&states, "web.fghj.internal").map(|t| t.port),
            Some(5001)
        );
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
        assert_eq!(
            resolve_route(&none, "web.fghj.internal").map(|t| t.port),
            None
        );
        assert!(wildcard_suffixes(&none).is_empty());
        assert!(raw_endpoints(&none).is_empty());
    }
}
