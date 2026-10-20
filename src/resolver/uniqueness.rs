//! Checks that the names derived from `node.id` (DNS names, container names)
//! stay as unique as the ids: concatenation (`{dep}.{owner}` vs
//! `{port}.{node}`) and `sanitize_label` (`a-b.c` vs `a.b.c`) both collide.

use std::collections::BTreeMap;

use super::graph::Node;
use super::warning::Warning;
use crate::util::label::sanitize_label;

/// Carried so the warning can say *why* two nodes want one name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    Domain,
    NamedPort,
    AdditionalHost,
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

/// Runs after `Node.domain` is filled in, so not during the traversal.
pub(crate) fn check_derived_name_collisions(nodes: &[Node]) -> Vec<Warning> {
    let mut warnings = Vec::new();
    warnings.extend(check_domains(nodes));
    warnings.extend(check_container_names(nodes));
    warnings
}

/// Everything `state::query::resolve_route` scans; a duplicate is a silent
/// first-match-wins misroute.
fn check_domains(nodes: &[Node]) -> Vec<Warning> {
    let mut claims: BTreeMap<String, Vec<(&str, Claim)>> = BTreeMap::new();
    for node in nodes {
        // A stub only owns its domain once pulled.
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
            local_path: None,
            domain: domain.into(),
            downloaded: true,
            dirty: false,
            head: None,
            flows: vec![],
            includes: Default::default(),
            build: None,
            ports: Default::default(),
            environment: vec![],
            command: vec![],
            volumes: vec![],
            additional_hosts: vec![],
            wildcard_hosts: vec![],
            env_file: vec![],
            restart: "no".into(),
            stop_signal: None,
            stop_grace_period: 10,
            user: None,
            working_dir: None,
            labels: Default::default(),
            cap_add: vec![],
            cap_drop: vec![],
            privileged: false,
            extra_hosts: vec![],
            healthcheck: None,
            platform: None,
            run_policy: None,
            migrates: None,
            seeds: Vec::new(),
            debug: None,
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

    #[test]
    fn stub_services_are_not_held_to_the_check() {
        let mut a = node("a.r", "same.ws.fghj.internal");
        a.downloaded = false;
        let mut b = node("b.r", "same.ws.fghj.internal");
        b.downloaded = false;
        assert!(check_derived_name_collisions(&[a, b]).is_empty());
    }
}
