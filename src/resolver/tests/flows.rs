//! Flows — public start lists. See `concepts/flows-v2.md`.

use super::write_yaml;
use crate::resolver::*;

fn blocking(graph: &Graph, needle: &str) -> bool {
    graph
        .warnings
        .iter()
        .any(|w| w.is_blocking() && w.message.contains(needle))
}

fn start(graph: &Graph, flow: Option<&str>) -> Vec<String> {
    let mut ids = graph.start_ids(flow).unwrap();
    ids.sort();
    ids
}

const BILLING: &str = r#"version: "2.0"
services:
  postgres:
    image: postgres:16
  search:
    image: elasticsearch:8
  queue:
    image: rabbitmq:3
  api:
    build: .
    depends_on:
      postgres: {}
      search: {required: false}
  indexer:
    build: .
    depends_on: [queue, search]
flows:
  pricing: [api]
  reindex: [indexer]
  db: [postgres]
"#;

const SHOP: &str = r#"version: "2.0"
include:
  billing: https://example.com/billing.git
services:
  db:
    image: postgres:16
  web:
    build: .
    depends_on: [db]
    environment:
      PRICING: http://${FGHJ_SERVICE_FQDN:billing/api}
flows:
  checkout: [web, billing/pricing]
  everything: [web, billing]
"#;

fn workspace() -> (tempfile::TempDir, Graph) {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(tmp.path(), "billing", BILLING);
    write_yaml(tmp.path(), "shop", SHOP);
    let graph = resolve_universe(tmp.path()).unwrap();
    (tmp, graph)
}

/// Case 1: the flow starts its members and what they can't start without —
/// and nothing that's only optional, so billing's search stays stopped.
#[test]
fn a_flow_starts_its_members_and_their_required_dependencies() {
    let (_tmp, graph) = workspace();
    assert!(graph.warnings.is_empty(), "{:?}", graph.warnings);
    assert_eq!(
        start(&graph, Some("shop/checkout")),
        ["api.billing", "db.shop", "postgres.billing", "web.shop"]
    );
}

/// Case 2: no flow is everything, as in production.
#[test]
fn no_flow_starts_everything() {
    let (_tmp, graph) = workspace();
    assert_eq!(start(&graph, None).len(), graph.nodes.len());
}

/// A bare alias is all of that repo.
#[test]
fn an_alias_in_a_flow_is_the_whole_repo() {
    let (_tmp, graph) = workspace();
    let ids = start(&graph, Some("shop/everything"));
    for id in [
        "indexer.billing",
        "queue.billing",
        "search.billing",
        "web.shop",
    ] {
        assert!(ids.iter().any(|i| i == id), "{id} missing from {ids:?}");
    }
}

#[test]
fn flows_are_listed_and_tagged_on_nodes_and_edges() {
    let (_tmp, graph) = workspace();
    assert_eq!(
        graph.flows,
        [
            "billing/db",
            "billing/pricing",
            "billing/reindex",
            "shop/checkout",
            "shop/everything"
        ]
    );
    let api = graph.nodes.iter().find(|n| n.id == "api.billing").unwrap();
    assert!(api.flows.contains(&"shop/checkout".to_string()));
    let e = graph
        .edges
        .iter()
        .find(|e| e.from == "api.billing" && e.to == "postgres.billing")
        .unwrap();
    assert!(e.flows.contains(&"billing/pricing".to_string()));
}

#[test]
fn an_unknown_flow_is_an_error_not_an_empty_run() {
    let (_tmp, graph) = workspace();
    let err = graph.start_ids(Some("checkout")).unwrap_err().to_string();
    assert!(err.contains("shop/checkout"), "{err}");
}

/// Case 3: a runtime dependency the run doesn't start is said before it
/// starts — once, as the flow, not once per member.
#[test]
fn a_runtime_dependency_outside_the_run_is_an_advisory() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(tmp.path(), "billing", BILLING);
    write_yaml(
        tmp.path(),
        "shop",
        r#"version: "2.0"
include:
  billing: https://example.com/billing.git
services:
  web:
    build: .
    environment:
      PRICING: http://${FGHJ_SERVICE_FQDN:billing/api}
    depends_on:
      billing/pricing: {required: false}
flows:
  checkout: [web, billing/db]
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    let ids = graph.start_ids(Some("shop/checkout")).unwrap();
    assert!(!ids.contains(&"api.billing".to_string()), "{ids:?}");
    let advisories = graph.start_advisories(&ids);
    assert_eq!(advisories.len(), 1, "{advisories:?}");
    assert!(
        advisories[0].contains("'web.shop' needs 'billing/pricing' at runtime"),
        "{advisories:?}"
    );
    // In the full run it is started, so nothing to say.
    assert!(
        graph
            .start_advisories(&graph.start_ids(None).unwrap())
            .is_empty()
    );
}

/// Case 14: flows naming each other across repos terminate, and each is
/// expanded fully.
#[test]
fn flows_referencing_each_other_across_repos_terminate() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "a",
        r#"version: "2.0"
include:
  b: https://example.com/b.git
services:
  x:
    build: .
flows:
  f: [x, b/g]
"#,
    );
    write_yaml(
        tmp.path(),
        "b",
        r#"version: "2.0"
include:
  a: https://example.com/a.git
services:
  y:
    build: .
flows:
  g: [y, a/f]
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(graph.warnings.is_empty(), "{:?}", graph.warnings);
    assert_eq!(start(&graph, Some("a/f")), ["x.a", "y.b"]);
    assert_eq!(start(&graph, Some("b/g")), ["x.a", "y.b"]);
}

/// A bad entry is reported once, by the flow that contains it, however
/// many flows reach it.
#[test]
fn a_bad_flow_entry_is_reported_once_by_its_own_flow() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "app",
        r#"version: "2.0"
services:
  api:
    build: .
flows:
  core: [api, nope]
  all: [core]
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    let bad: Vec<_> = graph
        .warnings
        .iter()
        .filter(|w| w.message.contains("'nope'"))
        .collect();
    assert_eq!(bad.len(), 1, "{bad:?}");
    assert!(bad[0].message.starts_with("flow 'app/core'"));
}

#[test]
fn a_flow_naming_another_repos_service_is_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(tmp.path(), "billing", BILLING);
    write_yaml(
        tmp.path(),
        "shop",
        r#"version: "2.0"
include:
  billing: https://example.com/billing.git
services:
  web:
    build: .
flows:
  checkout: [web, billing/api]
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        blocking(
            &graph,
            "flow 'shop/checkout' names 'billing/api', which is a service"
        ),
        "{:?}",
        graph.warnings
    );
}

/// Case 11: a flow the checked-out branch doesn't have.
#[test]
fn a_missing_foreign_flow_lists_what_the_repo_has() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(tmp.path(), "billing", BILLING);
    write_yaml(
        tmp.path(),
        "shop",
        r#"version: "2.0"
include:
  billing: https://example.com/billing.git
services:
  web:
    build: .
flows:
  checkout: [web, billing/invoices]
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        blocking(
            &graph,
            "has no flow named 'invoices'; use one of its flows: billing/db"
        ),
        "{:?}",
        graph.warnings
    );
}

/// An empty flow still exists — naming it isn't a typo.
#[test]
fn an_empty_flow_is_known() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "app",
        r#"version: "2.0"
services:
  api:
    build: .
flows:
  nothing: []
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(graph.start_ids(Some("app/nothing")).unwrap().is_empty());
}
