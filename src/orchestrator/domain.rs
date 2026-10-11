use crate::util::label::sanitize_label;

/// `Http` is proxied and the same address inside and outside the network;
/// `Raw` resolves straight to a container IP, for raw TCP. See `dns.rs`.
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
