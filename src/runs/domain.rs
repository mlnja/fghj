use super::naming::DEFAULT_RUN_ID;
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

/// Derives a node's canonical domain in the given zone for a given run — the
/// single definition `start_node` uses when actually launching a container,
/// also called from `resolver::resolve_universe` (always with
/// `DEFAULT_RUN_ID` and `DomainZone::Http`) so `Node.domain` can carry a
/// node's default-run address before any container for it has ever been
/// started. Two nodes can never collide on the result: `node_id` is already
/// the unique, leaf-first id (see `resolver::visit_local_service`/
/// `visit_dependency`), and `run_id` is folded in for every run except the
/// default one (see `start_node`'s own comment for why).
pub fn derive_domain(
    node_id: &str,
    domain_scope: &str,
    workspace_name: &str,
    run_id: &str,
    zone: DomainZone,
) -> String {
    let workspace = sanitize_label(workspace_name);
    let suffix = zone.suffix();
    if domain_scope == "stable" || run_id == DEFAULT_RUN_ID {
        format!("{node_id}.{workspace}.{suffix}")
    } else {
        format!("{node_id}.{run_id}.{workspace}.{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_domain_picks_the_suffix_for_the_requested_zone() {
        assert_eq!(
            derive_domain("svc", "run", "demo", DEFAULT_RUN_ID, DomainZone::Http),
            "svc.demo.fghj.internal"
        );
        assert_eq!(
            derive_domain("svc", "run", "demo", DEFAULT_RUN_ID, DomainZone::Raw),
            "svc.demo.fghj.raw.internal"
        );
        // Non-default run id, non-stable scope: run id folds into both zones
        // identically, only the suffix differs.
        assert_eq!(
            derive_domain("svc", "run", "demo", "feature-x", DomainZone::Http),
            "svc.feature-x.demo.fghj.internal"
        );
        assert_eq!(
            derive_domain("svc", "run", "demo", "feature-x", DomainZone::Raw),
            "svc.feature-x.demo.fghj.raw.internal"
        );
        // `stable` scope folds out the run id in both zones the same way.
        assert_eq!(
            derive_domain("svc", "stable", "demo", "feature-x", DomainZone::Raw),
            "svc.demo.fghj.raw.internal"
        );
    }
}
