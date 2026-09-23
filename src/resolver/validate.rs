//! Declared-port validation: the warnings a component earns for an
//! inconsistent `ports` block.

//! `ResolveCtx` — the traversal that walks components and their
//! dependencies, turning parsed config into graph nodes and edges.

use std::collections::BTreeMap;

use super::port::PortConfig;
use super::service::ServiceConfig;

use super::visit::ResolveCtx;

impl<'a> ResolveCtx<'a> {
    /// Warns (non-fatally) when a node declares more than one `primary` port
    /// — at most one port can sit at the node's own derived domain — or a
    /// `wildcard` port that's neither `primary` nor `name`d, so there's no
    /// domain for the wildcard to apply to. Shared between services and
    /// backing dependencies, since both use the same `ports` map shape.
    /// Everything `check_http_routes` used to check (a route naming a port
    /// the service never declared) is now structurally impossible: port and
    /// role are one `ports` map entry, not two lists to keep in sync.
    pub(crate) fn check_port_config(&mut self, id: &str, ports: &BTreeMap<String, PortConfig>) {
        let primaries: Vec<&str> = ports
            .iter()
            .filter(|(_, cfg)| cfg.primary)
            .map(|(port, _)| port.as_str())
            .collect();
        if primaries.len() > 1 {
            self.warnings.push(format!(
                "'{id}' declares more than one primary port ({}); only one can sit at its own domain",
                primaries.join(", ")
            ));
        }
        for (port, cfg) in ports {
            if cfg.wildcard && !cfg.primary && cfg.name.is_none() {
                self.warnings.push(format!(
                    "'{id}' port {port} sets wildcard but is neither primary nor named; there's no domain to wildcard"
                ));
            }
        }
    }

    pub(crate) fn check_ports(&mut self, service_id: &str, service: &ServiceConfig) {
        self.check_port_config(service_id, &service.ports);
        let has_primary = service.ports.values().any(|cfg| cfg.primary);
        if !service.additional_hosts.is_empty() && !has_primary {
            self.warnings.push(format!(
                "'{service_id}' declares additional_hosts but no primary port; those hosts won't be routed to anything"
            ));
        }
    }
}
