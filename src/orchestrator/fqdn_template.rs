use super::domain::{DomainZone, derive_domain};
use crate::resolver::{Graph, Node};

/// Expands `${FGHJ_SERVICE_FQDN[:path]}` (raw zone) and
/// `${FGHJ_SERVICE_FQDN_HTTP[:path]}` (proxied zone) in an env value. An
/// unresolvable token is left as is rather than failing the start.
pub(crate) fn expand_service_fqdn_templates(
    value: &str,
    node: &Node,
    own_raw_domain: &str,
    own_http_domain: &str,
    graph: &Graph,
) -> String {
    // `TOKEN_RAW` is a prefix of `TOKEN_HTTP`, so finding it finds the leftmost of
    // either; `starts_with` then tells them apart.
    const TOKEN_HTTP: &str = "${FGHJ_SERVICE_FQDN_HTTP";
    const TOKEN_RAW: &str = "${FGHJ_SERVICE_FQDN";
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find(TOKEN_RAW) {
        let (token_len, zone, own_domain) = if rest[start..].starts_with(TOKEN_HTTP) {
            (TOKEN_HTTP.len(), DomainZone::Http, own_http_domain)
        } else {
            (TOKEN_RAW.len(), DomainZone::Raw, own_raw_domain)
        };
        out.push_str(&rest[..start]);
        let Some(end_rel) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let end = start + end_rel;
        let inner = &rest[start + token_len..end]; // "" or ":name"
        let resolved = match inner.strip_prefix(':') {
            None => Some(own_domain.to_string()),
            Some(name) => sibling_domain(node, name, graph, zone),
        };
        match resolved {
            Some(domain) => out.push_str(&domain),
            None => out.push_str(&rest[start..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Every `path` named by an FQDN template in `value`. The resolver refuses
/// unknown ones so a typo fails at resolve time.
pub fn fqdn_template_paths(value: &str) -> Vec<String> {
    const TOKEN_RAW: &str = "${FGHJ_SERVICE_FQDN";
    let mut paths = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find(TOKEN_RAW) {
        let after = &rest[start + TOKEN_RAW.len()..];
        let after = after.strip_prefix("_HTTP").unwrap_or(after);
        let Some(end) = after.find('}') else { break };
        if let Some(path) = after[..end].strip_prefix(':') {
            paths.push(path.to_string());
        }
        rest = &after[end + 1..];
    }
    paths
}

/// The domain `${FGHJ_SERVICE_FQDN:path}` means for `node`: `name` (same repo)
/// or `alias/name` (a repo `node`'s repo includes). `None` if not found.
pub(crate) fn sibling_domain(
    node: &Node,
    path: &str,
    graph: &Graph,
    zone: DomainZone,
) -> Option<String> {
    let local_path = node.local_path.as_deref()?;
    let id = crate::resolver::visit::reference_target_id(local_path, &node.includes, path)?;
    let sibling = graph.nodes.iter().find(|n| n.id == id)?;
    Some(derive_domain(&sibling.id, &graph.workspace_name, zone))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::testing::{test_graph, test_node};

    #[test]
    fn expand_service_fqdn_templates_resolves_self_reference() {
        let php = test_node("php.app", "php", "service");
        let graph = test_graph(vec![php.clone()], vec![]);
        let out = expand_service_fqdn_templates(
            "https://${FGHJ_SERVICE_FQDN}/",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
        );
        assert_eq!(out, "https://php.app.shop.fghj.raw.internal/");
    }

    #[test]
    fn expand_service_fqdn_templates_http_variant_resolves_the_proxied_self_reference() {
        let php = test_node("php.app", "php", "service");
        let graph = test_graph(vec![php.clone()], vec![]);
        let out = expand_service_fqdn_templates(
            "https://${FGHJ_SERVICE_FQDN_HTTP}/",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
        );
        assert_eq!(out, "https://php.app.shop.fghj.internal/");
    }

    #[test]
    fn expand_service_fqdn_templates_resolves_a_service_in_the_same_repo() {
        let php = test_node("php.app", "php", "service");
        let mysql = test_node("mysql.app", "mysql", "backing");
        let graph = test_graph(vec![php.clone(), mysql], vec![]);
        let out = expand_service_fqdn_templates(
            "mysql://${FGHJ_SERVICE_FQDN:mysql}:3306/app",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
        );
        assert_eq!(out, "mysql://mysql.app.shop.fghj.raw.internal:3306/app");
    }

    #[test]
    fn expand_service_fqdn_templates_http_variant_resolves_a_sibling() {
        let php = test_node("php.app", "php", "service");
        let mysql = test_node("mysql.app", "mysql", "backing");
        let graph = test_graph(vec![php.clone(), mysql], vec![]);
        let out = expand_service_fqdn_templates(
            "https://${FGHJ_SERVICE_FQDN_HTTP:mysql}/",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
        );
        assert_eq!(out, "https://mysql.app.shop.fghj.internal/");
    }

    #[test]
    fn expand_service_fqdn_templates_resolves_an_included_repos_service() {
        let mut php = test_node("php.app", "php", "service");
        php.includes.insert("billing".into(), "billing-svc".into());
        let api = test_node("api.billing-svc", "api", "service");
        let graph = test_graph(vec![php.clone(), api], vec![]);
        let out = expand_service_fqdn_templates(
            "http://${FGHJ_SERVICE_FQDN:billing/api}",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
        );
        assert_eq!(out, "http://api.billing-svc.shop.fghj.raw.internal");
    }

    #[test]
    fn expand_service_fqdn_templates_does_not_search_other_repos_for_a_bare_name() {
        let php = test_node("php.app", "php", "service");
        let api = test_node("api.billing", "api", "service");
        let graph = test_graph(vec![php.clone(), api], vec![]);
        let out = expand_service_fqdn_templates(
            "${FGHJ_SERVICE_FQDN:api}",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
        );
        assert_eq!(out, "${FGHJ_SERVICE_FQDN:api}");
    }

    #[test]
    fn fqdn_template_paths_lists_every_named_path() {
        assert_eq!(
            fqdn_template_paths(
                "A=${FGHJ_SERVICE_FQDN}/${FGHJ_SERVICE_FQDN:db}/${FGHJ_SERVICE_FQDN_HTTP:billing/api}"
            ),
            vec!["db".to_string(), "billing/api".to_string()]
        );
        assert!(fqdn_template_paths("${FGHJ_SERVICE_FQDN:unterminated").is_empty());
    }

    #[test]
    fn expand_service_fqdn_templates_leaves_unknown_sibling_and_malformed_token_untouched() {
        let php = test_node("php.app", "php", "service");
        let graph = test_graph(vec![php.clone()], vec![]);

        let unknown = expand_service_fqdn_templates(
            "${FGHJ_SERVICE_FQDN:nope}",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
        );
        assert_eq!(unknown, "${FGHJ_SERVICE_FQDN:nope}");

        let unterminated = expand_service_fqdn_templates(
            "prefix ${FGHJ_SERVICE_FQDN no closing brace",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
        );
        assert_eq!(unterminated, "prefix ${FGHJ_SERVICE_FQDN no closing brace");
    }
}
