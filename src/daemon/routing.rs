//! Wiring the registry into the TLS proxy and the DNS server as their
//! source of live routes and zones.
//!
//! Both are live *read* paths — a request or query is already in flight and
//! needs an answer now — so neither is an `effects::Effect`. They still read
//! the same reducer-owned state the effects converge from, via
//! `WorkspaceRegistry::states` and the `state::query` projections, so a name
//! the proxy will route is exactly a name DNS will answer for.

use std::net::Ipv4Addr;

use crate::daemon::registry::WorkspaceRegistry;
use crate::state::query;
use crate::web::proxy;
use crate::{dns, raw_net};

impl proxy::RouteResolver for WorkspaceRegistry {
    fn resolve(&self, host: &str) -> Option<proxy::Backend> {
        query::resolve_route(&self.states(), host).map(|port| proxy::Backend {
            host: "127.0.0.1".to_string(),
            port,
        })
    }
}

impl dns::ZoneSource for WorkspaceRegistry {
    fn answer_for(&self, qname: &str) -> Option<Ipv4Addr> {
        if dns::in_zone(qname)
            || query::wildcard_suffixes(&self.states())
                .iter()
                .any(|z| dns::matches_zone(qname, z))
        {
            Some(dns::ANSWER)
        } else if dns::matches_zone(qname, dns::ZONE_RAW) {
            Some(raw_net::resolve(qname))
        } else {
            None
        }
    }
}
