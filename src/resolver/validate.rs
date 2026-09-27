//! Declared-config validation: the warnings a component earns for a `ports`
//! block or a `#RunOptions` field that cannot be carried out as written.

//! `ResolveCtx` — the traversal that walks components and their
//! dependencies, turning parsed config into graph nodes and edges.

use std::collections::BTreeMap;

use super::port::PortConfig;
use super::service::ServiceConfig;

use super::visit::ResolveCtx;
use super::warning::Warning;

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
            // Blocking: exactly one port can sit at the node's own domain,
            // so a second `primary` is an instruction that cannot be carried
            // out — and which of the two wins is not something the author
            // chose.
            self.warnings.push(Warning::blocking(format!(
                "'{id}' declares more than one primary port ({}); only one can sit at its own domain",
                primaries.join(", ")
            )));
        }
        for (port, cfg) in ports {
            if cfg.wildcard && !cfg.primary && cfg.name.is_none() {
                self.warnings.push(Warning::advisory(format!(
                    "'{id}' port {port} sets wildcard but is neither primary nor named; there's no domain to wildcard"
                )));
            }
        }
    }

    /// Warns (blockingly) when `stop_signal` isn't a signal name Docker will
    /// accept. Docker also takes a bare number or an unprefixed name
    /// ("TERM"), but `#RunOptions.stop_signal` requires the `SIG` form and so
    /// does this check — the two agree exactly, and an author who writes
    /// "TERM" gets told what to write instead rather than having CUE and the
    /// daemon disagree about it.
    ///
    /// Blocking rather than advisory: Docker fails `create_container`
    /// outright on an unknown signal, so the node would not start either way
    /// — this just says so before a container is attempted, with the field
    /// name in the message instead of Docker's own bare "invalid signal".
    pub(crate) fn check_stop_signal(&mut self, id: &str, stop_signal: Option<&str>) {
        let Some(signal) = stop_signal else { return };
        let well_formed = signal.starts_with("SIG")
            && signal.len() > 3
            && signal[3..]
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
        if !well_formed {
            self.warnings.push(Warning::blocking(format!(
                "'{id}' sets stop_signal '{signal}', which is not a signal name — write the \
                 SIG-prefixed uppercase form, e.g. SIGTERM or SIGQUIT"
            )));
        }
    }

    pub(crate) fn check_ports(&mut self, service_id: &str, service: &ServiceConfig) {
        self.check_port_config(service_id, &service.ports);
        self.check_stop_signal(service_id, service.stop_signal.as_deref());
        let has_primary = service.ports.values().any(|cfg| cfg.primary);
        if !service.additional_hosts.is_empty() && !has_primary {
            self.warnings.push(Warning::advisory(format!(
                "'{service_id}' declares additional_hosts but no primary port; those hosts won't be routed to anything"
            )));
        }
    }
}
