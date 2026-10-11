use crate::util::label::sanitize_label;

/// Which of the two zones a derived domain belongs to — see `dns.rs`'s
/// module doc for the full split. `Http` is the proxy/SNI-dispatched,
/// same-address-in-or-out zone (`fghj.internal`, unchanged from before this
/// split existed); `Raw` is the new in-network-only zone
/// (`fghj.raw.internal`) that resolves straight to a container's own IP via
/// Docker's native per-network DNS, for callers that need a real port
/// number raw TCP can't safely multiplex behind one shared address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainZone {
    Http,
    Raw,
}

impl DomainZone {
    fn suffix(self) -> &'static str {
        match self {
            DomainZone::Http => "fghj.internal",
            DomainZone::Raw => "fghj.raw.internal",
        }
    }
}

/// Derives a node's canonical domain in the given zone — the single
/// definition `start_node` uses when actually launching a container, also
/// called from `resolver::resolve_universe` so `Node.domain` can carry a
/// node's address before any container for it has ever been started. Two
/// nodes can never collide on the result: `node_id` is already the unique,
/// leaf-first id (see `resolver::visit_local_service`/`visit_dependency`).
pub fn derive_domain(node_id: &str, workspace_name: &str, zone: DomainZone) -> String {
    format!(
        "{node_id}.{}.{}",
        sanitize_label(workspace_name),
        zone.suffix()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_domain_picks_the_suffix_for_the_requested_zone() {
        assert_eq!(
            derive_domain("svc", "demo", DomainZone::Http),
            "svc.demo.fghj.internal"
        );
        assert_eq!(
            derive_domain("svc", "demo", DomainZone::Raw),
            "svc.demo.fghj.raw.internal"
        );
    }
}
