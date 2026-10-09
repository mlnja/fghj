//! Tasks — a service something waits on with
//! `condition: service_completed_successfully`. See `concepts/flows-v2.md`.

use super::write_yaml;
use crate::resolver::*;

fn task_repo(task_body: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "api",
        &format!(
            r#"version: "2.0"
services:
  db:
    image: postgres:16
  api:
    build: .
    depends_on:
      migrate: {{condition: service_completed_successfully}}
  migrate:
{task_body}"#
        ),
    );
    tmp
}

fn blocking(graph: &Graph, needle: &str) -> bool {
    graph
        .warnings
        .iter()
        .any(|w| w.is_blocking() && w.message.contains(needle))
}

/// A migration is the repo's own code with another command: it declares
/// the same `build`, and the waiter's condition is what makes it a task.
#[test]
fn completed_successfully_makes_the_target_a_task() {
    let tmp = task_repo(
        r#"    build: .
    command: ["rake", "db:migrate"]
    depends_on: [db]
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    let task = graph.nodes.iter().find(|n| n.id == "migrate.api").unwrap();

    assert_eq!(task.kind, "task");
    assert_eq!(task.command, vec!["rake", "db:migrate"]);
    assert!(task.build.is_some());
    assert_eq!(task.restart, "no");
    assert_eq!(task.run_policy.as_deref(), Some("on_start"));
    assert!(graph.warnings.is_empty(), "{:?}", graph.warnings);

    // db -> migrate -> api is the start order.
    let ids: Vec<String> = graph.nodes.iter().map(|n| n.id.clone()).collect();
    let order = crate::runs::topological_start_order(&ids, &graph.edges);
    let pos = |id: &str| order.iter().position(|o| o == id).unwrap();
    assert!(pos("db.api") < pos("migrate.api"));
    assert!(pos("migrate.api") < pos("api.api"));
}

#[test]
fn run_policy_round_trips() {
    let tmp = task_repo(
        r#"    build: .
    command: ["seed"]
    run: once
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    let task = graph.nodes.iter().find(|n| n.id == "migrate.api").unwrap();
    assert_eq!(task.run_policy.as_deref(), Some("once"));
}

#[test]
fn a_task_with_no_command_is_blocking() {
    let tmp = task_repo("    build: .\n");
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        blocking(&graph, "declares no `command`"),
        "{:?}",
        graph.warnings
    );
}

#[test]
fn a_task_with_a_healthcheck_or_restart_is_blocking() {
    let tmp = task_repo(
        r#"    build: .
    command: ["seed"]
    restart: always
    healthcheck:
      test: ["CMD", "true"]
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        blocking(&graph, "declares a healthcheck"),
        "{:?}",
        graph.warnings
    );
    assert!(blocking(&graph, "sets `restart`"), "{:?}", graph.warnings);
}

/// `run` only means something for a task, so declaring it makes one: the
/// way a seed nothing waits on — only listed in a flow — is still read as
/// a task, and its clean exit as success.
#[test]
fn run_alone_makes_a_service_a_task() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "api",
        r#"version: "2.0"
services:
  seed:
    build: .
    command: ["./seed"]
    run: once
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    let seed = graph.nodes.iter().find(|n| n.id == "seed.api").unwrap();
    assert_eq!(seed.kind, "task");
    assert_eq!(seed.run_policy.as_deref(), Some("once"));
    assert!(graph.blocking_warnings().is_empty(), "{:?}", graph.warnings);
}

/// Exit 0 is success for a task and drift for anything else, so one
/// service can't be waited on both ways.
#[test]
fn waiting_on_a_task_any_other_way_is_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "api",
        r#"version: "2.0"
services:
  api:
    build: .
    depends_on:
      migrate: {condition: service_completed_successfully}
  worker:
    build: .
    depends_on: [migrate]
  migrate:
    build: .
    command: ["migrate"]
"#,
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        blocking(
            &graph,
            "'worker.api' waits on 'migrate.api' with `service_started`"
        ),
        "{:?}",
        graph.warnings
    );
}
