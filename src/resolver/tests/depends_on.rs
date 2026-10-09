//! `depends_on` and hostname references. See `concepts/flows-v2.md`.

use super::{git_init, write_yaml};
use crate::resolver::*;

fn blocking(graph: &Graph, needle: &str) -> bool {
    graph
        .warnings
        .iter()
        .any(|w| w.is_blocking() && w.message.contains(needle))
}

fn edge<'g>(graph: &'g Graph, from: &str, to: &str, kind: &str) -> Option<&'g Edge> {
    graph
        .edges
        .iter()
        .find(|e| e.from == from && e.to == to && e.kind == kind)
}

/// The shape B — billing — has in the proposal: a database, a cache it can
/// start without, an api, and two published flows.
const BILLING: &str = r#"version: "2.0"
services:
  postgres:
    image: postgres:16
    healthcheck:
      test: ["CMD", "pg_isready"]
  search:
    image: elasticsearch:8
  api:
    build: .
    depends_on:
      postgres: {condition: service_healthy}
      search: {required: false}
flows:
  pricing: [api]
  db: [postgres]
"#;

#[test]
fn a_backing_service_is_its_repos_not_another_services() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(tmp.path(), "billing", BILLING);
    let graph = resolve_universe(tmp.path()).unwrap();

    let postgres = graph
        .nodes
        .iter()
        .find(|n| n.id == "postgres.billing")
        .unwrap();
    assert_eq!(postgres.kind, "backing");
    assert_eq!(postgres.local_path.as_deref(), Some("billing"));

    let e = edge(&graph, "api.billing", "postgres.billing", "depends-on").unwrap();
    assert!(e.required);
    assert_eq!(e.condition.as_deref(), Some("service_healthy"));
    assert!(
        !edge(&graph, "api.billing", "search.billing", "depends-on")
            .unwrap()
            .required
    );
    assert!(graph.warnings.is_empty(), "{:?}", graph.warnings);
}

#[test]
fn service_healthy_on_a_target_without_a_healthcheck_is_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "app",
        r#"version: "2.0"
services:
  db:
    image: postgres:16
  api:
    build: .
    depends_on:
      db: {condition: service_healthy}
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        blocking(&graph, "declares no healthcheck"),
        "{:?}",
        graph.warnings
    );
}

#[test]
fn a_service_needs_exactly_one_of_build_and_image() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "app",
        r#"version: "2.0"
services:
  both:
    build: .
    image: nginx
  neither:
    command: ["true"]
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        blocking(&graph, "'both.app' sets both"),
        "{:?}",
        graph.warnings
    );
    assert!(
        blocking(&graph, "'neither.app' sets neither"),
        "{:?}",
        graph.warnings
    );
}

#[test]
fn an_unknown_target_and_a_self_dependency_are_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "app",
        r#"version: "2.0"
services:
  api:
    build: .
    depends_on: [api, nope]
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        blocking(&graph, "'api.app' depends on itself"),
        "{:?}",
        graph.warnings
    );
    assert!(
        blocking(&graph, "no service and no include named 'nope'"),
        "{:?}",
        graph.warnings
    );
}

#[test]
fn within_a_repo_depends_on_names_services_not_flows() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "app",
        r#"version: "2.0"
services:
  api:
    build: .
    depends_on: [core]
flows:
  core: [api]
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        blocking(&graph, "which is a flow in this repo"),
        "{:?}",
        graph.warnings
    );
}

/// Case 4: waiting on another repo's flow is one edge per member, labelled
/// with the flow it came from.
#[test]
fn depending_on_another_repos_flow_waits_on_its_members() {
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
    depends_on:
      billing/db: {condition: service_healthy}
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    let e = edge(&graph, "web.shop", "postgres.billing", "depends-on").unwrap();
    assert_eq!(e.via_flow.as_deref(), Some("billing/db"));
    assert!(graph.warnings.is_empty(), "{:?}", graph.warnings);
}

/// Waiting on a whole repo is its own services, not what it includes —
/// two repos including each other and waiting on each other's repo is the
/// mutual wait, not each waiting on itself.
#[test]
fn depending_on_a_repo_waits_on_its_own_services_only() {
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
    depends_on: [b]
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
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(edge(&graph, "x.a", "y.b", "depends-on").is_some());
    assert!(edge(&graph, "x.a", "x.a", "depends-on").is_none());
    assert!(graph.warnings.is_empty(), "{:?}", graph.warnings);
}

/// The encapsulation rule: B's services are B's internals, and the error
/// says what B does publish.
#[test]
fn naming_another_repos_service_is_blocking_and_lists_its_flows() {
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
    depends_on: [billing/api]
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        blocking(
            &graph,
            "billing's services are internal; use one of its flows: billing/db, billing/pricing"
        ),
        "{:?}",
        graph.warnings
    );
}

#[test]
fn completed_successfully_across_repos_is_blocking() {
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
    depends_on:
      billing/db: {condition: service_completed_successfully}
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        blocking(&graph, "it never completes"),
        "{:?}",
        graph.warnings
    );
}

/// Case 6: A waits on B's flow and B waits on A's — it can never start.
#[test]
fn a_cross_repo_wait_cycle_is_blocking() {
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
    depends_on: [b/core]
flows:
  core: [x]
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
    depends_on: [a/core]
flows:
  core: [y]
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(blocking(&graph, "dependency cycle"), "{:?}", graph.warnings);
}

/// An include resolves by the repo's real git remote, so the folder can be
/// called anything.
#[test]
fn an_include_finds_a_repo_checked_out_under_another_name() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path(), "billing-svc", "https://example.com/billing.git");
    write_yaml(tmp.path(), "billing-svc", BILLING);
    write_yaml(
        tmp.path(),
        "shop",
        r#"version: "2.0"
include:
  billing: https://example.com/billing.git
services:
  web:
    build: .
    depends_on: [billing/db]
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(edge(&graph, "web.shop", "postgres.billing-svc", "depends-on").is_some());
    assert!(!graph.nodes.iter().any(|n| !n.downloaded));
}

/// A repo not on disk is one stub, however many repos include it, and
/// nothing can be checked against what it declares.
#[test]
fn an_included_repo_not_on_disk_is_one_stub() {
    let tmp = tempfile::tempdir().unwrap();
    for repo in ["shop", "admin"] {
        write_yaml(
            tmp.path(),
            repo,
            r#"version: "2.0"
include:
  billing:
    repo: https://example.com/billing.git
    default_branch: main
services:
  web:
    build: .
    depends_on: [billing/pricing]
    environment:
      API: http://${FGHJ_SERVICE_FQDN:billing/api}
"#,
        );
    }
    let graph = resolve_universe(tmp.path()).unwrap();
    let stubs: Vec<&Node> = graph.nodes.iter().filter(|n| !n.downloaded).collect();
    assert_eq!(stubs.len(), 1);
    assert_eq!(stubs[0].id, "billing");
    assert_eq!(stubs[0].branch.as_deref(), Some("main"));
    assert!(edge(&graph, "web.shop", "billing", "depends-on").is_some());
    assert!(graph.warnings.is_empty(), "{:?}", graph.warnings);
}

#[test]
fn an_include_alias_colliding_with_a_service_is_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "shop",
        r#"version: "2.0"
include:
  web: https://example.com/web.git
services:
  web:
    build: .
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        blocking(&graph, "both as an include alias"),
        "{:?}",
        graph.warnings
    );
}

/// Rule 8: a hostname is sugar, not an edge — two services naming each
/// other's hostnames (case 5) have no dependency at all until they declare
/// one.
#[test]
fn hostname_references_are_not_edges() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "billing",
        r#"version: "2.0"
include:
  shop: https://example.com/shop.git
services:
  api:
    build: .
    environment:
      WEBHOOK: http://${FGHJ_SERVICE_FQDN_HTTP:shop/web}/hook
"#,
    );
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
      API: http://${FGHJ_SERVICE_FQDN:billing/api}
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(graph.edges.is_empty(), "{:?}", graph.edges);
    assert!(graph.warnings.is_empty(), "{:?}", graph.warnings);
}

#[test]
fn a_hostname_naming_nothing_is_blocking() {
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
      - DB=${FGHJ_SERVICE_FQDN:dbb}
      - API=${FGHJ_SERVICE_FQDN:billing/apii}
      - X=${FGHJ_SERVICE_FQDN:nobody/api}
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(blocking(&graph, "references 'dbb'"), "{:?}", graph.warnings);
    assert!(
        blocking(&graph, "references 'billing/apii'"),
        "{:?}",
        graph.warnings
    );
    assert!(
        blocking(&graph, "references 'nobody/api'"),
        "{:?}",
        graph.warnings
    );
}
