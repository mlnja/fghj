//! Whole-workspace scenarios: several `.fghj.yaml` files resolved together,
//! checked for what a run starts, in which order, and which contradictions
//! refuse it. The per-feature modules test one rule each; these test the
//! rules composing.
//!
//! A start plan is asserted as *waves*: the run starts exactly the nodes
//! listed, and everything in one wave starts before anything in the next.
//! Order inside a wave is deliberately unspecified.

use super::write_yaml;
use crate::resolver::*;
use crate::runs::topological_start_order;

fn workspace(repos: &[(&str, &str)]) -> (tempfile::TempDir, Graph) {
    let tmp = tempfile::tempdir().unwrap();
    for (name, yaml) in repos {
        write_yaml(tmp.path(), name, yaml);
    }
    let graph = resolve_universe(tmp.path()).unwrap();
    (tmp, graph)
}

/// The order a run of `flow` would start its nodes in.
fn plan(graph: &Graph, flow: Option<&str>) -> Vec<String> {
    let ids = graph.start_ids(flow).unwrap();
    topological_start_order(&ids, &graph.edges)
}

fn assert_waves(plan: &[String], waves: &[&[&str]]) {
    let mut expected: Vec<&str> = waves.iter().flat_map(|w| w.iter().copied()).collect();
    expected.sort_unstable();
    let mut actual: Vec<&str> = plan.iter().map(String::as_str).collect();
    actual.sort_unstable();
    assert_eq!(actual, expected, "the run starts the wrong set: {plan:?}");

    let pos = |id: &str| plan.iter().position(|p| p == id).unwrap();
    for pair in waves.windows(2) {
        for earlier in pair[0] {
            for later in pair[1] {
                assert!(
                    pos(earlier) < pos(later),
                    "'{earlier}' must start before '{later}': {plan:?}"
                );
            }
        }
    }
}

/// `id` starts after every one of `deps`, for a node that isn't in one
/// wave with anything else.
fn assert_after(plan: &[String], id: &str, deps: &[&str]) {
    let pos = |id: &str| plan.iter().position(|p| p == id).unwrap();
    for dep in deps {
        assert!(
            pos(dep) < pos(id),
            "'{dep}' must start before '{id}': {plan:?}"
        );
    }
}

fn assert_clean(graph: &Graph) {
    assert!(graph.warnings.is_empty(), "{:#?}", graph.warnings);
}

/// Every reported cycle, as its `a -> b -> a` text.
fn cycles(graph: &Graph) -> Vec<String> {
    graph
        .warnings
        .iter()
        .filter(|w| w.is_blocking() && w.message.starts_with("dependency cycle"))
        .map(|w| w.message.clone())
        .collect()
}

fn blocking(graph: &Graph, needle: &str) -> bool {
    graph
        .warnings
        .iter()
        .any(|w| w.is_blocking() && w.message.contains(needle))
}

// ---------------------------------------------------------------- one repo

/// A typical app: two databases, a migration, a seed that needs the
/// migrated schema (a task by `run:`, only one journey wants it), an API
/// and a worker sharing the database, and a web front that waits on the
/// API.
const SHOP: &str = r#"version: "2.0"
services:
  db:
    image: postgres:16
    healthcheck:
      test: ["CMD", "pg_isready"]
  cache:
    image: redis:7
  migrate:
    build: .
    command: ["migrate"]
    depends_on:
      db: {condition: service_healthy}
  seed:
    build: .
    command: ["seed"]
    run: on_start
    depends_on:
      migrate: {condition: service_completed_successfully}
  api:
    build: .
    depends_on:
      db: {condition: service_healthy}
      cache: {}
      migrate: {condition: service_completed_successfully}
  worker:
    build: .
    depends_on: [db, cache]
  web:
    build: .
    depends_on: [api]
flows:
  storefront: [web]
  background: [worker]
  demo: [web, seed]
"#;

#[test]
fn a_full_run_starts_everything_dependencies_first() {
    let (_tmp, graph) = workspace(&[("shop", SHOP)]);
    assert_clean(&graph);
    let mut full = plan(&graph, None);
    assert_after(&full, "worker.shop", &["db.shop", "cache.shop"]);
    assert_after(&full, "seed.shop", &["migrate.shop"]);
    full.retain(|id| id != "worker.shop" && id != "seed.shop");
    assert_waves(
        &full,
        &[
            &["db.shop", "cache.shop"],
            &["migrate.shop"],
            &["api.shop"],
            &["web.shop"],
        ],
    );
}

#[test]
fn tasks_are_marked_and_services_are_not() {
    let (_tmp, graph) = workspace(&[("shop", SHOP)]);
    let kind = |id: &str| {
        graph
            .nodes
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.kind.clone())
            .unwrap()
    };
    assert_eq!(kind("migrate.shop"), "task");
    assert_eq!(kind("seed.shop"), "task");
    assert_ne!(kind("api.shop"), "task");
    assert_ne!(kind("worker.shop"), "task");
}

/// Nothing needs `seed` to start, so the storefront flow leaves it out, and
/// the worker, which nothing in the flow needs either.
#[test]
fn a_flow_drops_optional_dependencies_and_unrelated_services() {
    let (_tmp, graph) = workspace(&[("shop", SHOP)]);
    assert_waves(
        &plan(&graph, Some("shop/storefront")),
        &[
            &["db.shop", "cache.shop"],
            &["migrate.shop"],
            &["api.shop"],
            &["web.shop"],
        ],
    );
}

/// Listing the seed in a flow brings it in, after what *it* needs. It's
/// not ordered against `api`: nothing declared that one needs the other.
#[test]
fn a_seed_listed_in_a_flow_starts_after_what_it_needs() {
    let (_tmp, graph) = workspace(&[("shop", SHOP)]);
    let mut demo = plan(&graph, Some("shop/demo"));
    assert_after(&demo, "seed.shop", &["migrate.shop"]);
    demo.retain(|id| id != "seed.shop");
    assert_waves(
        &demo,
        &[
            &["db.shop", "cache.shop"],
            &["migrate.shop"],
            &["api.shop"],
            &["web.shop"],
        ],
    );
}

/// A runtime dependency orders nothing and brings nothing in. Listed in
/// the flow, it starts with no ordering against its dependent; left out,
/// the start says so.
#[test]
fn a_runtime_dependency_is_started_only_when_listed_and_never_ordered() {
    let (_tmp, graph) = workspace(&[(
        "app",
        r#"version: "2.0"
services:
  hooks:
    build: .
  api:
    build: .
    depends_on:
      hooks: {required: false}
flows:
  lean: [api]
  full: [api, hooks]
"#,
    )]);
    assert_clean(&graph);

    let lean = plan(&graph, Some("app/lean"));
    assert_eq!(lean, vec!["api.app"]);
    assert_eq!(
        graph.start_advisories(&lean),
        vec!["'api.app' needs 'hooks.app' at runtime, but this run doesn't start it"]
    );

    // Sorted, not dependency-ordered: "api" < "hooks" alphabetically, so a
    // runtime edge that ordered anything would have put hooks first.
    let full = plan(&graph, Some("app/full"));
    assert_eq!(full, vec!["api.app", "hooks.app"]);
    assert!(graph.start_advisories(&full).is_empty());
}

/// "Must have finished" is a start-time statement, so a runtime
/// dependency can't say it.
#[test]
fn a_runtime_dependency_that_must_complete_is_blocking() {
    let (_tmp, graph) = workspace(&[(
        "app",
        r#"version: "2.0"
services:
  seed:
    build: .
    command: ["seed"]
  api:
    build: .
    depends_on:
      seed: {condition: service_completed_successfully, required: false}
"#,
    )]);
    assert!(
        blocking(&graph, "marks it `required: false`"),
        "{:?}",
        graph.warnings
    );
}

#[test]
fn a_flow_of_one_leaf_service_starts_its_dependencies_only() {
    let (_tmp, graph) = workspace(&[("shop", SHOP)]);
    assert_waves(
        &plan(&graph, Some("shop/background")),
        &[&["db.shop", "cache.shop"], &["worker.shop"]],
    );
}

/// The start set is the same whichever member a flow lists first, and a
/// flow can name another flow of its own repo.
#[test]
fn flows_compose_within_a_repo() {
    let (_tmp, graph) = workspace(&[(
        "app",
        r#"version: "2.0"
services:
  db:
    image: postgres:16
  api:
    build: .
    depends_on: [db]
  admin:
    build: .
    depends_on: [db]
  docs:
    build: .
flows:
  core: [api]
  backoffice: [core, admin]
  everything-but-docs: [backoffice]
"#,
    )]);
    assert_clean(&graph);
    assert_waves(
        &plan(&graph, Some("app/everything-but-docs")),
        &[&["db.app"], &["api.app", "admin.app"]],
    );
}

/// Two services that call each other: `required: false` in both
/// directions, never a cycle, and both still wait on what they need to
/// start. Their hostname templates add nothing to the graph.
#[test]
fn services_calling_each_other_are_not_a_cycle() {
    let (_tmp, graph) = workspace(&[(
        "app",
        r#"version: "2.0"
services:
  db:
    image: postgres:16
  orders:
    build: .
    depends_on:
      db: {}
      payments: {required: false}
    environment:
      PAYMENTS: http://${FGHJ_SERVICE_FQDN:payments}
  payments:
    build: .
    depends_on:
      db: {}
      orders: {required: false}
    environment:
      ORDERS: http://${FGHJ_SERVICE_FQDN:orders}
"#,
    )]);
    assert_clean(&graph);
    assert_waves(
        &plan(&graph, None),
        &[&["db.app"], &["orders.app", "payments.app"]],
    );
    let runtime = graph.edges.iter().filter(|e| !e.required).count();
    assert_eq!(runtime, 2, "{:?}", graph.edges);
}

/// Nodes with no dependencies at all start in a stable (sorted) order, so
/// two runs of the same graph start the same way.
#[test]
fn independent_services_start_in_a_stable_order() {
    let yaml = r#"version: "2.0"
services:
  zeta:
    image: busybox
  alpha:
    image: busybox
  mid:
    image: busybox
"#;
    let (_tmp, graph) = workspace(&[("app", yaml)]);
    assert_eq!(plan(&graph, None), ["alpha.app", "mid.app", "zeta.app"]);
    let (_tmp2, graph2) = workspace(&[("app", yaml)]);
    assert_eq!(plan(&graph, None), plan(&graph2, None));
}

// ------------------------------------------------------------------ cycles

#[test]
fn a_two_service_cycle_is_one_blocking_warning() {
    let (_tmp, graph) = workspace(&[(
        "app",
        r#"version: "2.0"
services:
  a:
    build: .
    depends_on: [b]
  b:
    build: .
    depends_on: [a]
"#,
    )]);
    let found = cycles(&graph);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("a.app -> b.app -> a.app"), "{found:?}");
    assert!(graph.refuse_if_blocked().is_err());
}

#[test]
fn a_three_service_ring_names_every_member_in_order() {
    let (_tmp, graph) = workspace(&[(
        "app",
        r#"version: "2.0"
services:
  a:
    build: .
    depends_on: [b]
  b:
    build: .
    depends_on: [c]
  c:
    build: .
    depends_on: [a]
  d:
    build: .
    depends_on: [a]
"#,
    )]);
    let found = cycles(&graph);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("a.app -> b.app -> c.app -> a.app"),
        "{found:?}"
    );
    // `d` hangs off the ring without being part of it.
    assert!(!found[0].contains("d.app"), "{found:?}");
}

/// Two separate loops are two warnings, so both get fixed in one go.
#[test]
fn independent_cycles_are_reported_separately() {
    let (_tmp, graph) = workspace(&[(
        "app",
        r#"version: "2.0"
services:
  a:
    build: .
    depends_on: [b]
  b:
    build: .
    depends_on: [a]
  x:
    build: .
    depends_on: [y]
  y:
    build: .
    depends_on: [x]
"#,
    )]);
    assert_eq!(cycles(&graph).len(), 2, "{:?}", graph.warnings);
}

/// A runtime dependency orders nothing, so a loop through one deadlocks
/// nothing and isn't a cycle.
#[test]
fn a_loop_through_a_runtime_dependency_is_not_a_cycle() {
    let (_tmp, graph) = workspace(&[(
        "app",
        r#"version: "2.0"
services:
  a:
    build: .
    depends_on:
      b: {required: false}
  b:
    build: .
    depends_on: [a]
"#,
    )]);
    assert_clean(&graph);
    assert_eq!(plan(&graph, None), vec!["a.app", "b.app"]);
}

/// The classic mistake: the migration waits on the API it migrates for.
#[test]
fn a_task_waiting_on_its_own_waiter_is_a_cycle() {
    let (_tmp, graph) = workspace(&[(
        "app",
        r#"version: "2.0"
services:
  api:
    build: .
    depends_on:
      migrate: {condition: service_completed_successfully}
  migrate:
    build: .
    command: ["migrate"]
    depends_on: [api]
"#,
    )]);
    assert_eq!(cycles(&graph).len(), 1, "{:?}", graph.warnings);
}

/// A diamond converges but doesn't loop.
#[test]
fn a_diamond_is_not_a_cycle() {
    let (_tmp, graph) = workspace(&[(
        "app",
        r#"version: "2.0"
services:
  base:
    image: postgres:16
  left:
    build: .
    depends_on: [base]
  right:
    build: .
    depends_on: [base]
  top:
    build: .
    depends_on: [left, right]
"#,
    )]);
    assert_clean(&graph);
    assert_waves(
        &plan(&graph, None),
        &[&["base.app"], &["left.app", "right.app"], &["top.app"]],
    );
}

// --------------------------------------------------------------- many repos

/// shop -> billing -> ledger, each through a flow. Each repo exposes only
/// a slice of itself, so a checkout run starts only those slices.
const LEDGER: &str = r#"version: "2.0"
services:
  ledger-db:
    image: postgres:16
  ledger:
    build: .
    depends_on: [ledger-db]
  audit:
    build: .
    depends_on: [ledger-db]
flows:
  core: [ledger]
  auditing: [audit]
"#;

const BILLING: &str = r#"version: "2.0"
include:
  ledger: https://example.com/ledger.git
services:
  postgres:
    image: postgres:16
  api:
    build: .
    depends_on: [postgres, ledger/core]
  indexer:
    build: .
    depends_on: [postgres]
flows:
  pricing: [api]
  reindex: [indexer]
"#;

const STOREFRONT: &str = r#"version: "2.0"
include:
  billing: https://example.com/billing.git
services:
  db:
    image: postgres:16
  web:
    build: .
    depends_on: [db, billing/pricing]
    environment:
      PRICING: http://${FGHJ_SERVICE_FQDN:billing/api}
flows:
  checkout: [web]
"#;

fn three_tiers() -> (tempfile::TempDir, Graph) {
    workspace(&[
        ("ledger", LEDGER),
        ("billing", BILLING),
        ("storefront", STOREFRONT),
    ])
}

#[test]
fn a_flow_reaches_through_two_repos_and_starts_deepest_first() {
    let (_tmp, graph) = three_tiers();
    assert_clean(&graph);
    let mut checkout = plan(&graph, Some("storefront/checkout"));
    // Storefront's own database waits on nothing; only web needs it.
    assert_after(&checkout, "web.storefront", &["db.storefront"]);
    checkout.retain(|id| id != "db.storefront");
    assert_waves(
        &checkout,
        &[
            &["ledger-db.ledger"],
            &["ledger.ledger", "postgres.billing"],
            &["api.billing"],
            &["web.storefront"],
        ],
    );
}

#[test]
fn the_full_run_includes_what_no_flow_reaches() {
    let (_tmp, graph) = three_tiers();
    let all = plan(&graph, None);
    for id in ["audit.ledger", "indexer.billing"] {
        assert!(all.iter().any(|p| p == id), "{id} missing: {all:?}");
    }
    assert_eq!(all.len(), graph.nodes.len());
}

/// Cross-repo waits are recorded per member, with the flow they came
/// through, so the UI and the start order see the same thing.
#[test]
fn a_cross_repo_wait_is_an_edge_per_flow_member() {
    let (_tmp, graph) = three_tiers();
    let via: Vec<&Edge> = graph
        .edges
        .iter()
        .filter(|e| e.via_flow.as_deref() == Some("billing/pricing"))
        .collect();
    assert_eq!(via.len(), 1, "{via:?}");
    assert_eq!(
        (via[0].from.as_str(), via[0].to.as_str()),
        ("web.storefront", "api.billing")
    );
}

/// Each flow of the deepest repo is its own start set: one flow doesn't
/// pull in the others.
#[test]
fn a_repos_own_flows_stay_independent() {
    let (_tmp, graph) = three_tiers();
    assert_waves(
        &plan(&graph, Some("ledger/auditing")),
        &[&["ledger-db.ledger"], &["audit.ledger"]],
    );
    assert_waves(
        &plan(&graph, Some("billing/reindex")),
        &[&["postgres.billing"], &["indexer.billing"]],
    );
}

/// Two repos depending on one shared repo get one copy of it, started
/// once, before either of them.
#[test]
fn two_dependents_share_one_instance() {
    let admin = r#"version: "2.0"
include:
  billing: https://example.com/billing.git
services:
  admin:
    build: .
    depends_on: [billing/pricing]
flows:
  ops: [admin]
"#;
    let (_tmp, graph) = workspace(&[
        ("ledger", LEDGER),
        ("billing", BILLING),
        ("storefront", STOREFRONT),
        ("admin", admin),
        (
            "everything",
            r#"version: "2.0"
include:
  shop: https://example.com/storefront.git
  admin: https://example.com/admin.git
services: {}
flows:
  both: [shop/checkout, admin/ops]
"#,
        ),
    ]);
    assert!(
        graph.blocking_warnings().is_empty(),
        "{:#?}",
        graph.warnings
    );
    let count = graph
        .nodes
        .iter()
        .filter(|n| n.label == "api" && n.local_path.as_deref() == Some("billing"))
        .count();
    assert_eq!(count, 1);
    let mut both = plan(&graph, Some("everything/both"));
    assert_after(&both, "web.storefront", &["db.storefront"]);
    both.retain(|id| id != "db.storefront");
    assert_waves(
        &both,
        &[
            &["ledger-db.ledger"],
            &["ledger.ledger", "postgres.billing"],
            &["api.billing"],
            &["web.storefront", "admin.admin"],
        ],
    );
}

/// Waiting on a whole repo waits on its own services but not on what it
/// includes — those are its services' business — while a flow entry for
/// the whole repo starts everything it pulls in.
#[test]
fn a_bare_alias_waits_shallow_but_a_flow_alias_starts_deep() {
    let (_tmp, graph) = workspace(&[
        ("ledger", LEDGER),
        ("billing", BILLING),
        (
            "shop",
            r#"version: "2.0"
include:
  billing: https://example.com/billing.git
services:
  web:
    build: .
    depends_on: [billing]
flows:
  all: [web, billing]
"#,
        ),
    ]);
    assert_clean(&graph);
    let waits: Vec<&str> = graph
        .edges
        .iter()
        .filter(|e| e.kind == "depends-on" && e.from == "web.shop")
        .map(|e| e.to.as_str())
        .collect();
    assert!(waits.contains(&"api.billing"), "{waits:?}");
    assert!(waits.contains(&"indexer.billing"), "{waits:?}");
    assert!(!waits.iter().any(|w| w.ends_with(".ledger")), "{waits:?}");

    let all = plan(&graph, Some("shop/all"));
    assert!(all.iter().any(|p| p == "audit.ledger"), "{all:?}");
}

/// Repos that call each other and include each other are fine as long as
/// only one direction waits.
#[test]
fn mutual_includes_with_one_way_waits_resolve() {
    let (_tmp, graph) = workspace(&[
        (
            "orders",
            r#"version: "2.0"
include:
  payments: https://example.com/payments.git
services:
  orders:
    build: .
    depends_on: [payments/checkout]
flows:
  api: [orders]
"#,
        ),
        (
            "payments",
            r#"version: "2.0"
include:
  orders: https://example.com/orders.git
services:
  charge:
    build: .
    environment:
      WEBHOOK: http://${FGHJ_SERVICE_FQDN:orders/orders}
flows:
  checkout: [charge]
"#,
        ),
    ]);
    assert_clean(&graph);
    assert_waves(
        &plan(&graph, Some("orders/api")),
        &[&["charge.payments"], &["orders.orders"]],
    );
}

/// A ring across three repos, each waiting on the next one's flow.
#[test]
fn a_three_repo_ring_is_a_cycle() {
    let repo = |next: &str| {
        format!(
            r#"version: "2.0"
include:
  {next}: https://example.com/{next}.git
services:
  svc:
    build: .
    depends_on: [{next}/main]
flows:
  main: [svc]
"#
        )
    };
    let (a, b, c) = (repo("b"), repo("c"), repo("a"));
    let (_tmp, graph) = workspace(&[("a", &a), ("b", &b), ("c", &c)]);
    let found = cycles(&graph);
    assert_eq!(found.len(), 1, "{found:?}");
    for id in ["svc.a", "svc.b", "svc.c"] {
        assert!(found[0].contains(id), "{found:?}");
    }
}

/// A flow that waits on another repo's flow through a third repo's flow:
/// all three slices start, in order.
#[test]
fn flows_nest_across_repos() {
    let (_tmp, graph) = workspace(&[
        ("ledger", LEDGER),
        ("billing", BILLING),
        (
            "qa",
            r#"version: "2.0"
include:
  billing: https://example.com/billing.git
  ledger: https://example.com/ledger.git
services:
  e2e:
    build: .
    depends_on: [billing/pricing]
flows:
  smoke: [e2e, ledger/auditing]
"#,
        ),
    ]);
    assert_clean(&graph);
    let mut smoke = plan(&graph, Some("qa/smoke"));
    // Listed in the flow but needed by nothing in it: it only waits on its
    // own database.
    assert_after(&smoke, "audit.ledger", &["ledger-db.ledger"]);
    smoke.retain(|id| id != "audit.ledger");
    assert_waves(
        &smoke,
        &[
            &["ledger-db.ledger"],
            &["ledger.ledger", "postgres.billing"],
            &["api.billing"],
            &["e2e.qa"],
        ],
    );
}

// ------------------------------------------------------------ not pulled yet

/// Before billing is cloned, it's one stub node: the flow reaches it,
/// waits on it, and it's first in line.
#[test]
fn an_unpulled_repo_is_a_stub_ordered_before_its_dependents() {
    let (_tmp, graph) = workspace(&[("storefront", STOREFRONT)]);
    assert_clean(&graph);
    let stub = graph.nodes.iter().find(|n| n.id == "billing").unwrap();
    assert!(!stub.downloaded);
    assert_waves(
        &plan(&graph, Some("storefront/checkout")),
        &[&["billing", "db.storefront"], &["web.storefront"]],
    );
}

/// Once billing is on disk, its own includes become the next stubs —
/// which is how pulling walks a workspace one layer at a time.
#[test]
fn pulling_one_layer_reveals_the_next() {
    let (_tmp, graph) = workspace(&[("storefront", STOREFRONT), ("billing", BILLING)]);
    assert_clean(&graph);
    let stubs: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| !n.downloaded)
        .map(|n| n.id.as_str())
        .collect();
    assert_eq!(stubs, ["ledger"]);
    assert_waves(
        &plan(&graph, Some("storefront/checkout")),
        &[
            &["ledger", "postgres.billing", "db.storefront"],
            &["api.billing"],
            &["web.storefront"],
        ],
    );
}

// ------------------------------------------------------------ contradictions

/// Every independent mistake in one workspace is reported, not just the
/// first, and the start is refused listing all of them.
#[test]
fn several_mistakes_are_all_reported_at_once() {
    let (_tmp, graph) = workspace(&[
        ("ledger", LEDGER),
        ("billing", BILLING),
        (
            "shop",
            r#"version: "2.0"
include:
  billing: https://example.com/billing.git
services:
  web:
    build: .
    depends_on: [billing/api, nope]
    environment:
      X: http://${FGHJ_SERVICE_FQDN:ghost}
  a:
    build: .
    depends_on: [b]
  b:
    build: .
    depends_on: [a]
"#,
        ),
    ]);
    assert!(blocking(&graph, "'billing/api'"), "{:#?}", graph.warnings);
    assert!(blocking(&graph, "nope"), "{:#?}", graph.warnings);
    assert!(blocking(&graph, "ghost"), "{:#?}", graph.warnings);
    assert_eq!(cycles(&graph).len(), 1, "{:#?}", graph.warnings);
    let err = graph.refuse_if_blocked().unwrap_err().to_string();
    for needle in ["billing/api", "nope", "ghost", "dependency cycle"] {
        assert!(err.contains(needle), "{needle} missing from: {err}");
    }
}

/// A workspace with a mistake in one repo still resolves the rest, so the
/// UI can show it, and a flow elsewhere still has its start set.
#[test]
fn a_broken_repo_does_not_hide_the_rest() {
    let (_tmp, graph) = workspace(&[
        ("ledger", LEDGER),
        (
            "broken",
            r#"version: "2.0"
services:
  x:
    build: .
    depends_on: [missing]
"#,
        ),
    ]);
    assert!(blocking(&graph, "missing"), "{:#?}", graph.warnings);
    assert_waves(
        &plan(&graph, Some("ledger/core")),
        &[&["ledger-db.ledger"], &["ledger.ledger"]],
    );
}
