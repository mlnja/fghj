use super::write_yaml;
use crate::resolver::*;

fn blocking(graph: &Graph, needle: &str) -> bool {
    graph
        .warnings
        .iter()
        .any(|w| w.is_blocking() && w.message.contains(needle))
}

fn edge<'g>(graph: &'g Graph, from: &str, to: &str) -> Option<&'g Edge> {
    graph.edges.iter().find(|e| e.from == from && e.to == to)
}

fn node<'g>(graph: &'g Graph, id: &str) -> &'g Node {
    graph.nodes.iter().find(|n| n.id == id).unwrap()
}

const CORE: &str = r#"version: "2.0"
services:
  mysql:
    image: mysql:8
    healthcheck:
      test: ["CMD", "mysqladmin", "ping"]
  minio:
    image: minio/minio
  migrate:
    image: sqldef/mysqldef
    command: ["mysqldef"]
    migrates: mysql
  seed:
    image: mysql:8
    command: ["bash", "seed.sh"]
    seeds: [mysql, minio]
  php:
    build: .
    depends_on:
      mysql: {condition: service_healthy}
flows:
  app: [php]
  db: [mysql]
  demo: [php, seed]
"#;

fn core() -> (tempfile::TempDir, Graph) {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(tmp.path(), "core", CORE);
    let graph = resolve_universe(tmp.path()).unwrap();
    (tmp, graph)
}

#[test]
fn migrating_or_seeding_makes_a_task_that_waits_on_its_stores() {
    let (_tmp, graph) = core();
    assert!(graph.warnings.is_empty(), "{:?}", graph.warnings);

    let migrate = node(&graph, "migrate.core");
    assert_eq!(migrate.kind, "task");
    assert_eq!(migrate.migrates.as_deref(), Some("mysql.core"));
    let seed = node(&graph, "seed.core");
    assert_eq!(seed.kind, "task");
    assert_eq!(seed.seeds, vec!["mysql.core", "minio.core"]);

    let to_mysql = edge(&graph, "migrate.core", "mysql.core").unwrap();
    assert!(to_mysql.needed_to_start() && !to_mysql.implied);
    assert_eq!(to_mysql.condition.as_deref(), Some("service_healthy"));
    let to_minio = edge(&graph, "seed.core", "minio.core").unwrap();
    assert_eq!(to_minio.condition.as_deref(), Some("service_started"));
}

/// Nobody forgets to wait on the migration: waiting on the store is enough.
#[test]
fn whatever_waits_on_a_store_waits_on_its_migration() {
    let (_tmp, graph) = core();
    for from in ["php.core", "seed.core"] {
        let e = edge(&graph, from, "migrate.core").unwrap_or_else(|| panic!("{from}"));
        assert!(e.implied && e.needed_to_start());
        assert_eq!(
            e.condition.as_deref(),
            Some("service_completed_successfully")
        );
    }
    assert!(edge(&graph, "migrate.core", "migrate.core").is_none());

    let ids: Vec<String> = graph.nodes.iter().map(|n| n.id.clone()).collect();
    let order = crate::orchestrator::topological_start_order(&ids, &graph.edges);
    let pos = |id: &str| order.iter().position(|o| o == id).unwrap();
    assert!(pos("mysql.core") < pos("migrate.core"));
    assert!(pos("migrate.core") < pos("php.core"));
    assert!(pos("migrate.core") < pos("seed.core"));
}

/// The seed is a step in a journey, not part of the store: only a flow that
/// lists it starts it, and nothing waits on it.
#[test]
fn a_migration_starts_with_its_store_and_a_seed_only_when_listed() {
    let (_tmp, graph) = core();
    let start = |flow| {
        let mut ids = graph.start_ids(Some(flow)).unwrap();
        ids.sort();
        ids
    };
    assert_eq!(start("core/db"), vec!["migrate.core", "mysql.core"]);
    assert_eq!(
        start("core/app"),
        vec!["migrate.core", "mysql.core", "php.core"]
    );
    assert_eq!(
        start("core/demo"),
        vec![
            "migrate.core",
            "minio.core",
            "mysql.core",
            "php.core",
            "seed.core"
        ]
    );
    assert!(edge(&graph, "php.core", "seed.core").is_none());
}

#[test]
fn starting_a_store_on_its_own_brings_its_migration() {
    let (_tmp, graph) = core();
    let ids = graph.with_migrations(vec!["mysql.core".to_string()]);
    assert_eq!(ids, vec!["mysql.core", "migrate.core"]);
}

fn single(services: &str) -> Graph {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "core",
        &format!("version: \"2.0\"\nservices:\n  db:\n    image: postgres:16\n{services}"),
    );
    resolve_universe(tmp.path()).unwrap()
}

#[test]
fn a_store_must_be_a_service_in_the_same_repo() {
    let graph = single("  migrate:\n    image: x\n    command: [m]\n    migrates: nope\n");
    assert!(
        blocking(&graph, "has no service by that name"),
        "{:?}",
        graph.warnings
    );
    let graph = single("  seed:\n    image: x\n    command: [s]\n    seeds: [seed]\n");
    assert!(blocking(&graph, "names itself"), "{:?}", graph.warnings);
}

#[test]
fn a_store_has_one_migration() {
    let graph = single(
        "  a:\n    image: x\n    command: [m]\n    migrates: db\n  \
         b:\n    image: x\n    command: [m]\n    migrates: db\n",
    );
    assert!(
        blocking(&graph, "both migrate 'db.core'"),
        "{:?}",
        graph.warnings
    );
}

#[test]
fn run_or_both_kinds_at_once_is_blocking() {
    let graph = single("  m:\n    image: x\n    command: [m]\n    migrates: db\n    run: once\n");
    assert!(
        blocking(&graph, "sets `run` alongside `migrates`"),
        "{:?}",
        graph.warnings
    );
    let graph = single("  m:\n    image: x\n    command: [m]\n    migrates: db\n    seeds: [db]\n");
    assert!(
        blocking(&graph, "both `migrates` and `seeds`"),
        "{:?}",
        graph.warnings
    );
}

#[test]
fn a_task_cannot_be_a_store() {
    let graph = single(
        "  m:\n    image: x\n    command: [m]\n    migrates: db\n  \
         s:\n    image: x\n    command: [s]\n    seeds: [m]\n",
    );
    assert!(
        blocking(&graph, "'m.core' is a task"),
        "{:?}",
        graph.warnings
    );
}

/// A migration that waits on a service using its own store can't ever start.
#[test]
fn a_migration_waiting_on_its_stores_user_is_a_cycle() {
    let graph = single(
        "  api:\n    build: .\n    depends_on: [db]\n  \
         m:\n    image: x\n    command: [m]\n    migrates: db\n    depends_on: [api]\n",
    );
    assert!(
        graph
            .warnings
            .iter()
            .any(|w| w.is_blocking() && w.message.contains("cycle")),
        "{:?}",
        graph.warnings
    );
}

/// Nothing waits on a seed, so waiting on a flow that lists one isn't waiting
/// on it; a flow that starts the waiter still runs it.
#[test]
fn waiting_on_another_repos_flow_does_not_wait_on_its_seed() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "core",
        &CORE.replace("  app: [php]", "  app: [php, seed]"),
    );
    write_yaml(
        tmp.path(),
        "shop",
        r#"version: "2.0"
include:
  core: https://example.com/core.git
services:
  web:
    build: .
    depends_on:
      core/app: {condition: service_started}
  tool:
    build: .
    depends_on:
      core/app: {condition: service_started, required: false}
flows:
  shop: [web]
  tools: [tool]
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(edge(&graph, "web.shop", "php.core").is_some());
    assert!(edge(&graph, "web.shop", "seed.core").is_none());
    assert!(edge(&graph, "tool.shop", "seed.core").is_none());
    let seed = node(&graph, "seed.core");
    assert!(
        seed.flows.contains(&"shop/shop".to_string()),
        "{:?}",
        seed.flows
    );
    assert!(
        !seed.flows.contains(&"shop/tools".to_string()),
        "{:?}",
        seed.flows
    );
}
