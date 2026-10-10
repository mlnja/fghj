use super::domain::{DomainZone, derive_domain};
use crate::resolver::{Graph, Node};

/// Expands `${FGHJ_SERVICE_FQDN}`/`${FGHJ_SERVICE_FQDN:path}` (this node's
/// own, or another service's, `fghj.raw.internal` domain — see
/// `sibling_domain` for what `path` can look like) and `${FGHJ_SERVICE_FQDN_HTTP}`/
/// `${FGHJ_SERVICE_FQDN_HTTP:path}` (the `fghj.internal` — proxied — domain
/// instead) in a single `environment`/`env_file` value, so a CUE author can
/// reference a derived address without hand-computing `derive_domain`'s
/// formula into a literal string (the convention every hardcoded
/// `*_HOST`/`*_URL` value in `aikido-core`/`aikifactory`'s `.fghj.yaml`
/// followed before this existed). The bare `FQDN` form resolves to the raw
/// zone — direct container access — because that's what every real caller
/// of this macro today actually needs (a database connection string, a raw
/// S3 endpoint); `_HTTP` is the rare opt-in for a service's own *proxied*
/// identity (e.g. a presigned URL meant to be handed to something outside
/// the network). The longer `_HTTP` token is checked first so it's never
/// mistaken for the shorter one plus a literal `_HTTP` suffix. A manual scan
/// rather than the `regex` crate (not otherwise a dependency) — the grammar
/// is just those forms, simple enough that a scanner is less code than
/// pulling in a new crate. An unresolvable `:path` (no such sibling) or a
/// token missing its closing `}` is left untouched in the output rather
/// than erroring — a typo here shouldn't fail an entire run when the
/// literal fallback is at least diagnosable in logs, the same tolerance
/// `parse_env_file` extends to a malformed line. (One in `environment` never
/// gets here: the resolver refuses it — see `fqdn_template_paths`. Only an
/// `env_file` value, read at start, can.)
pub(crate) fn expand_service_fqdn_templates(
    value: &str,
    node: &Node,
    own_raw_domain: &str,
    own_http_domain: &str,
    graph: &Graph,
) -> String {
    // `TOKEN_RAW` is a literal prefix of `TOKEN_HTTP`, so `rest.find`ing it
    // always lands on the truly leftmost occurrence of either token — a
    // standalone `_HTTP` search alone would miss the case where the
    // leftmost token is actually the plain (raw) form, and searching both
    // separately would need extra tie-breaking since they can share a start
    // index. Whichever one `find` lands on, a cheap `starts_with` check at
    // that position tells the two apart.
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

/// Every `path` named by a `${FGHJ_SERVICE_FQDN:path}` or
/// `${FGHJ_SERVICE_FQDN_HTTP:path}` in `value`, in order — the bare
/// self-reference forms name nothing. The resolver refuses one that names
/// nothing, so a typo fails at resolve time instead of leaking a literal
/// into a container's environment. It never makes an edge: a hostname is
/// sugar, not a dependency.
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

/// The domain `${FGHJ_SERVICE_FQDN:path}` means from `node`'s own
/// `environment`. `path` is `name` — a service in `node`'s repo — or
/// `alias/name`, a service in a repo `node`'s repo includes. The alias is
/// looked up in `node.includes`, the folder it resolved to; nothing else is
/// searched, so the same path always means the same service no matter what
/// is running. `None` when the alias isn't one, or the service isn't in the
/// graph (a repo not pulled yet).
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
    use crate::runs::testing::{test_graph, test_node};

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

    /// `alias/name` goes through the repo's `include:` — the folder it
    /// points at, whatever it's called on disk.
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

    /// A bare name never reaches into another repo, however unambiguous
    /// it would be: the same path always means the same service.
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
