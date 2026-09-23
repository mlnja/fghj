use super::domain::{DomainZone, derive_domain};
use crate::resolver::{Graph, Node};

/// Expands `${FGHJ_SERVICE_FQDN}`/`${FGHJ_SERVICE_FQDN:path}` (this node's
/// own, or a sibling's, `fghj.raw.internal` domain — see `sibling_domain`
/// for what `path` can look like) and `${FGHJ_SERVICE_FQDN_HTTP}`/
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
/// `parse_env_file` extends to a malformed line.
pub(crate) fn expand_service_fqdn_templates(
    value: &str,
    node: &Node,
    own_raw_domain: &str,
    own_http_domain: &str,
    graph: &Graph,
    run_id: &str,
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
    loop {
        let Some(start) = rest.find(TOKEN_RAW) else {
            break;
        };
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
            Some(name) => sibling_domain(node, name, graph, run_id, zone),
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

/// Finds the domain `${FGHJ_SERVICE_FQDN:path}` means from `node`'s own
/// `environment`, in two ways (first match wins):
///
/// - A backing dependency matching `path` that shares an "owns" owner with
///   `node` — the owner is whoever's "owns" edge points at `node` (a
///   backing dependency looking for a sibling backing dependency), or
///   `node.id` itself if nothing owns it (a service looking up one of its
///   own directly-declared backing dependencies).
/// - A service matching `path` that `node` directly depends on via a
///   `kind: service` dependency (same-repo or cross-repo — both produce a
///   "depends-on" edge from `node.id`, see `resolver::visit_dependency`).
///   This is the only way to reach a sibling *service*: unlike backing
///   dependencies, services aren't owned, so there's no shared-owner case
///   to fall back on — only what `node` itself declares a dependency on.
///
/// `path` is one bare name (`mysql`) in the common case — matched against
/// just the candidate's own leaf name — or `::`-separated segments
/// (`aikifactory::aikifactory::minio`) for the rare case where that's
/// ambiguous. A node's `id` is already the leaf-first chain the domain
/// itself is built from (`{name}.{owner-id}`, see `resolver::visit_dependency`
/// /`visit_local_services`) — root-first is just easier to read/write, so
/// `path`'s segments are reversed and dot-joined into that same shape
/// before matching, e.g. `aikifactory::aikifactory::minio` becomes
/// `minio.aikifactory.aikifactory`, an exact prefix of the real id
/// `minio.aikifactory.aikifactory` (before the workspace/`fghj.internal`
/// suffix `derive_domain` appends). Fewer segments than the full id just
/// means "match any id with this as a trailing-toward-the-root prefix" —
/// as many as it takes to stop being ambiguous, no more.
pub(crate) fn sibling_domain(
    node: &Node,
    path: &str,
    graph: &Graph,
    run_id: &str,
    zone: DomainZone,
) -> Option<String> {
    let mut segments: Vec<&str> = path.split("::").collect();
    segments.reverse();
    let id_prefix = segments.join(".");
    let matches = |candidate: &Node| {
        candidate.id == id_prefix || candidate.id.starts_with(&format!("{id_prefix}."))
    };

    let owner_id = graph
        .edges
        .iter()
        .find(|e| e.kind == "owns" && e.to == node.id)
        .map(|e| e.from.as_str())
        .unwrap_or(node.id.as_str());
    let sibling = graph
        .edges
        .iter()
        .filter(|e| e.kind == "owns" && e.from == owner_id)
        .find_map(|e| graph.nodes.iter().find(|n| n.id == e.to && matches(n)))
        .or_else(|| {
            graph
                .edges
                .iter()
                .filter(|e| e.kind == "depends-on" && e.from == node.id)
                .find_map(|e| graph.nodes.iter().find(|n| n.id == e.to && matches(n)))
        })?;
    Some(derive_domain(
        &sibling.id,
        &sibling.domain_scope,
        &graph.workspace_name,
        run_id,
        zone,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::naming::DEFAULT_RUN_ID;
    use crate::runs::testing::{edge, test_graph, test_node};

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
            DEFAULT_RUN_ID,
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
            DEFAULT_RUN_ID,
        );
        assert_eq!(out, "https://php.app.shop.fghj.internal/");
    }

    #[test]
    fn expand_service_fqdn_templates_resolves_sibling_owned_by_a_service() {
        // php owns mysql; php's own environment references its sibling by name.
        let php = test_node("php.app", "php", "service");
        let mysql = test_node("mysql.php.app", "mysql", "backing");
        let graph = test_graph(
            vec![php.clone(), mysql],
            vec![edge("php.app", "mysql.php.app", "owns")],
        );
        let out = expand_service_fqdn_templates(
            "mysql://${FGHJ_SERVICE_FQDN:mysql}:3306/app",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(out, "mysql://mysql.php.app.shop.fghj.raw.internal:3306/app");
    }

    #[test]
    fn expand_service_fqdn_templates_http_variant_resolves_a_sibling() {
        let php = test_node("php.app", "php", "service");
        let mysql = test_node("mysql.php.app", "mysql", "backing");
        let graph = test_graph(
            vec![php.clone(), mysql],
            vec![edge("php.app", "mysql.php.app", "owns")],
        );
        let out = expand_service_fqdn_templates(
            "https://${FGHJ_SERVICE_FQDN_HTTP:mysql}/",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(out, "https://mysql.php.app.shop.fghj.internal/");
    }

    #[test]
    fn expand_service_fqdn_templates_resolves_sibling_owned_by_the_same_owner() {
        // phpmyadmin and mysql are both owned by php; phpmyadmin references
        // its sibling mysql, not anything it owns itself (it owns nothing).
        let php_id = "php.app";
        let mysql = test_node("mysql.php.app", "mysql", "backing");
        let phpmyadmin = test_node("phpmyadmin.php.app", "phpmyadmin", "backing");
        let graph = test_graph(
            vec![mysql, phpmyadmin.clone()],
            vec![
                edge(php_id, "mysql.php.app", "owns"),
                edge(php_id, "phpmyadmin.php.app", "owns"),
            ],
        );
        let out = expand_service_fqdn_templates(
            "${FGHJ_SERVICE_FQDN:mysql}",
            &phpmyadmin,
            "phpmyadmin.php.app.shop.fghj.raw.internal",
            "phpmyadmin.php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(out, "mysql.php.app.shop.fghj.raw.internal");
    }

    #[test]
    fn expand_service_fqdn_templates_resolves_a_directly_depended_on_sibling_service() {
        // vite depends on php (same-repo `kind: service`) — and the same
        // "depends-on" edge shape covers a cross-repo flow dependency, so
        // this also stands in for that case.
        let vite = test_node("vite.app", "vite", "service");
        let php = test_node("php.app", "php", "service");
        let graph = test_graph(
            vec![vite.clone(), php],
            vec![edge("vite.app", "php.app", "depends-on")],
        );
        let out = expand_service_fqdn_templates(
            "http://${FGHJ_SERVICE_FQDN:php}",
            &vite,
            "vite.app.shop.fghj.raw.internal",
            "vite.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(out, "http://php.app.shop.fghj.raw.internal");
    }

    #[test]
    fn expand_service_fqdn_templates_disambiguates_a_colliding_leaf_name_with_a_path() {
        // php owns a backing dependency named "mysql" *and* directly depends
        // on a cross-repo service that also happens to be named "mysql" —
        // the bare leaf name is ambiguous, so the backing dependency wins by
        // default (declared via "owns", checked first), and the qualified
        // root-first path (mirroring how the id itself, leaf-first, would
        // read as `mysql.otherrepo`) is needed to reach the other one.
        let php = test_node("php.app", "php", "service");
        let mysql_backing = test_node("mysql.php.app", "mysql", "backing");
        let mysql_service = test_node("mysql.otherrepo", "mysql", "service");
        let graph = test_graph(
            vec![php.clone(), mysql_backing, mysql_service],
            vec![
                edge("php.app", "mysql.php.app", "owns"),
                edge("php.app", "mysql.otherrepo", "depends-on"),
            ],
        );

        let bare = expand_service_fqdn_templates(
            "${FGHJ_SERVICE_FQDN:mysql}",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(bare, "mysql.php.app.shop.fghj.raw.internal");

        let qualified = expand_service_fqdn_templates(
            "${FGHJ_SERVICE_FQDN:otherrepo::mysql}",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(qualified, "mysql.otherrepo.shop.fghj.raw.internal");
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
            DEFAULT_RUN_ID,
        );
        assert_eq!(unknown, "${FGHJ_SERVICE_FQDN:nope}");

        let unterminated = expand_service_fqdn_templates(
            "prefix ${FGHJ_SERVICE_FQDN no closing brace",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(unterminated, "prefix ${FGHJ_SERVICE_FQDN no closing brace");
    }
}
