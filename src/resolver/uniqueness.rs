//! The check that the four namespaces fghj *derives* from `node.id` are as
//! injective as `node.id` itself is.
//!
//! `resolver::visit_local_service`/`visit_dependency` go to real trouble to
//! make ids unique: leaf-first, unconditionally qualified by the owning
//! scope. That uniqueness then gets projected into several flat codomains —
//! a DNS name, a Docker container name — and **the projections are not
//! injective**, so two distinct ids can land on one string. Nothing
//! downstream notices: `state::query::resolve_route` is a `find_map` over a
//! `BTreeMap` (first match by key order wins, silently), and Docker just
//! refuses the second container with an error that says nothing about
//! naming.
//!
//! The two ways it happens are structurally different and both are checked
//! here:
//!
//! - **Concatenation without a discriminator.** A backing dependency's
//!   domain is `{dep.name}.{owner_domain}` and a named port's alias is
//!   `{port.name}.{node_domain}` — the same shape, built independently, in
//!   one shared DNS codomain. A service with a backing dep `minio` *and* a
//!   port named `minio` claims one name twice.
//! - **Escaping that destroys the separator.** `derive_domain` keeps the
//!   id's dots (injectivity preserved); `container_name` runs it through
//!   `sanitize_label`, which collapses every non-alphanumeric run to `-`.
//!   Service names may contain hyphens, so `a-b.c` and `a.b.c` both become
//!   `a-b-c`.
//!
//! Both are [`Severity::Blocking`]: the config names something that cannot
//! be carried out, and starting anyway gives a run that is quietly not the
//! one the config describes — a service reachable at somebody else's domain,
//! or a node with no container at all. See `concepts/AUDIT.md` B1/B2.

use std::collections::BTreeMap;

use super::graph::Node;
use super::warning::Warning;
use crate::util::label::sanitize_label;

/// What made a node claim a name — carried so the warning can say *why* two
/// nodes want the same string, which is the part that is otherwise
/// impossible to guess from the string alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    /// The node's own derived domain (`derive_domain`).
    Domain,
    /// A `{name}.{node domain}` alias from a named port.
    NamedPort,
    /// A literal author-declared `additional_hosts` entry.
    AdditionalHost,
    /// An `additional_hosts` entry that also claims every subdomain.
    WildcardHost,
}

impl Claim {
    fn describe(self) -> &'static str {
        match self {
            Claim::Domain => "its own domain",
            Claim::NamedPort => "a named port",
            Claim::AdditionalHost => "additional_hosts",
            Claim::WildcardHost => "wildcard additional_hosts",
        }
    }
}

/// Every warning the derived namespaces earn. Returns them rather than
/// pushing into `ResolveCtx` because this runs in `resolve_universe`'s final
/// pass, after `Node.domain` has been filled in — the claims can't be
/// computed during the traversal, since a node's domain isn't known until
/// the workspace name is.
pub(crate) fn check_derived_name_collisions(nodes: &[Node]) -> Vec<Warning> {
    let mut warnings = Vec::new();
    warnings.extend(check_domains(nodes));
    warnings.extend(check_container_names(nodes));
    warnings
}

/// The DNS codomain `state::query::resolve_route` actually scans: a node's
/// own domain, its named-port aliases, and its literal host aliases, all in
/// one flat map. Anything claimed twice is a silent first-match-wins
/// misroute.
fn check_domains(nodes: &[Node]) -> Vec<Warning> {
    let mut claims: BTreeMap<String, Vec<(&str, Claim)>> = BTreeMap::new();
    for node in nodes {
        // A stub node has no resolved config yet, so it has nothing to
        // claim beyond a domain it will only really own once pulled.
        if !node.downloaded && node.kind == "service" {
            continue;
        }
        claims
            .entry(node.domain.clone())
            .or_default()
            .push((&node.id, Claim::Domain));
        for cfg in node.ports.values() {
            if let Some(name) = &cfg.name {
                claims
                    .entry(format!("{name}.{}", node.domain))
                    .or_default()
                    .push((&node.id, Claim::NamedPort));
            }
        }
        for host in &node.additional_hosts {
            claims
                .entry(host.clone())
                .or_default()
                .push((&node.id, Claim::AdditionalHost));
        }
        for suffix in &node.wildcard_hosts {
            claims
                .entry(suffix.clone())
                .or_default()
                .push((&node.id, Claim::WildcardHost));
        }
    }

    claims
        .into_iter()
        .filter(|(_, c)| c.len() > 1)
        .map(|(name, c)| {
            let who = c
                .iter()
                .map(|(id, claim)| format!("{id} ({})", claim.describe()))
                .collect::<Vec<_>>()
                .join(", ");
            Warning::blocking(format!(
                "'{name}' is claimed by more than one node ({who}); only one \
                 will receive its traffic, and which one is unspecified"
            ))
        })
        .collect()
}

/// `runs::node_spec` names a container
/// `fghj-{workspace}-{run}-{sanitize_label(node.id)}`. Two ids that sanitize
/// alike mean the second container fails to be created at all, with a Docker
/// name-conflict error that never mentions the node ids involved.
fn check_container_names(nodes: &[Node]) -> Vec<Warning> {
    let mut by_label: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for node in nodes {
        by_label
            .entry(sanitize_label(&node.id))
            .or_default()
            .push(&node.id);
    }

    by_label
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(label, ids)| {
            Warning::blocking(format!(
                "node ids {} all reduce to the container name suffix '{label}' \
                 (dots and hyphens both become '-'); only the first to start \
                 will get a container",
                ids.join(", ")
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::port::PortConfig;

    fn node(id: &str, domain: &str) -> Node {
        Node {
            id: id.into(),
            label: id.into(),
            kind: "service".into(),
            image: None,
            branch: None,
            repo: None,
            domain_scope: "run".into(),
            local_path: None,
            domain: domain.into(),
            downloaded: true,
            dirty: false,
            flows: vec![],
            build: None,
            ports: Default::default(),
            environment: vec![],
            command: vec![],
            volumes: vec![],
            additional_hosts: vec![],
            wildcard_hosts: vec![],
            env_file: vec![],
            restart: "no".into(),
            user: None,
            working_dir: None,
            labels: Default::default(),
            cap_add: vec![],
            cap_drop: vec![],
            privileged: false,
            extra_hosts: vec![],
            healthcheck: None,
            platform: None,
        }
    }

    fn named_port(name: &str) -> PortConfig {
        PortConfig {
            name: Some(super::super::name::Name::parse(name).expect("valid port name")),
            ..Default::default()
        }
    }

    #[test]
    fn a_clean_workspace_earns_no_warnings() {
        let nodes = vec![
            node("cart.shop", "cart.shop.ws.fghj.internal"),
            node("bff.shop", "bff.shop.ws.fghj.internal"),
        ];
        assert!(check_derived_name_collisions(&nodes).is_empty());
    }

    /// B1: the backing node `minio.cart.shop` and `cart.shop`'s port named
    /// `minio` derive the identical domain from two independent formulas.
    #[test]
    fn a_named_port_colliding_with_a_backing_dependency_is_caught() {
        let mut cart = node("cart.shop", "cart.shop.ws.fghj.internal");
        cart.ports.insert("9000".into(), named_port("minio"));
        let mut minio = node("minio.cart.shop", "minio.cart.shop.ws.fghj.internal");
        minio.kind = "backing".into();

        let warnings = check_derived_name_collisions(&[cart, minio]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0]
                .message
                .contains("minio.cart.shop.ws.fghj.internal"),
            "{warnings:?}"
        );
        assert!(warnings[0].message.contains("a named port"), "{warnings:?}");
        assert!(
            warnings[0].message.contains("its own domain"),
            "{warnings:?}"
        );
    }

    /// B2: `a-b.c` and `a.b.c` are distinct ids with distinct domains, and
    /// the same container name.
    #[test]
    fn ids_that_sanitize_alike_are_caught_even_though_their_domains_differ() {
        let nodes = vec![
            node("a-b.c", "a-b.c.ws.fghj.internal"),
            node("a.b.c", "a.b.c.ws.fghj.internal"),
        ];
        let warnings = check_derived_name_collisions(&nodes);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].message.contains("a-b-c"), "{warnings:?}");
    }

    /// The pass this one replaces only compared wildcard suffixes to each
    /// other; the same suffix declared twice must still be caught.
    #[test]
    fn two_nodes_declaring_one_wildcard_suffix_are_still_caught() {
        let mut a = node("a.r", "a.r.ws.fghj.internal");
        a.wildcard_hosts = vec!["tenant.example.test".into()];
        let mut b = node("b.r", "b.r.ws.fghj.internal");
        b.wildcard_hosts = vec!["tenant.example.test".into()];

        let warnings = check_derived_name_collisions(&[a, b]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].message.contains("tenant.example.test"),
            "{warnings:?}"
        );
    }

    /// A literal alias colliding with a derived domain was previously
    /// checked nowhere at all — neither name is a wildcard, so the old pass
    /// never looked.
    #[test]
    fn an_additional_host_colliding_with_a_derived_domain_is_caught() {
        let mut a = node("a.r", "a.r.ws.fghj.internal");
        a.additional_hosts = vec!["b.r.ws.fghj.internal".into()];
        let b = node("b.r", "b.r.ws.fghj.internal");

        let warnings = check_derived_name_collisions(&[a, b]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].message.contains("additional_hosts"),
            "{warnings:?}"
        );
    }

    /// A not-yet-pulled service is a placeholder — warning that its
    /// unresolved domain collides would fire on every workspace with two
    /// stubs before anyone had written a line of config.
    #[test]
    fn stub_services_are_not_held_to_the_check() {
        let mut a = node("a.r", "same.ws.fghj.internal");
        a.downloaded = false;
        let mut b = node("b.r", "same.ws.fghj.internal");
        b.downloaded = false;
        assert!(check_derived_name_collisions(&[a, b]).is_empty());
    }
}
