//! `#Task` — the terminating node kind. See `concepts/AUDIT.md` E1.

use super::write_component;
use crate::resolver::*;

/// A task declared with no `image` runs the owning service's own built
/// image — the common case, since a migration is that service's code with a
/// different command. Inheriting the owner's `build` is what makes that
/// work without the author declaring the same Dockerfile twice.
#[test]
fn a_task_inherits_the_owning_services_build_when_it_declares_no_image() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "api",
        "  build:\n\
         \x20   context: .\n\
         \x20   dockerfile: Dockerfile.api\n\
         \x20 dependencies:\n\
         \x20   - kind: task\n\
         \x20     name: migrate\n\
         \x20     command: [\"rake\", \"db:migrate\"]\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();
    let task = graph
        .nodes
        .iter()
        .find(|n| n.id == "migrate.api.api")
        .expect("task node");

    assert_eq!(task.kind, "task");
    assert_eq!(task.command, vec!["rake", "db:migrate"]);
    assert_eq!(task.image, None);
    assert_eq!(
        task.build.as_ref().map(|b| b.dockerfile.as_str()),
        Some("Dockerfile.api"),
        "an image-less task must build the owner's image, not nothing"
    );
    // Not routed to and not published: a task answers on no domain.
    assert!(task.ports.is_empty());
    assert!(task.additional_hosts.is_empty());
    // Forced regardless of what the owner declares — a restart policy on a
    // container whose purpose is to exit would restart it forever.
    assert_eq!(task.restart, "no");
    assert!(task.healthcheck.is_none());
    assert_eq!(task.run_policy.as_deref(), Some("on_start"));
}

/// The owner waits for the task (`owns`), and the task waits for the
/// sibling it names (`after`) — which is what makes `db -> migrate -> api`
/// the actual start order.
#[test]
fn after_orders_a_task_behind_a_sibling_backing_dependency() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "api",
        "  build:\n\
         \x20   context: .\n\
         \x20 dependencies:\n\
         \x20   - kind: backing\n\
         \x20     name: db\n\
         \x20     image: postgres:16\n\
         \x20   - kind: task\n\
         \x20     name: migrate\n\
         \x20     command: [\"rake\", \"db:migrate\"]\n\
         \x20     after: [\"db\"]\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        graph.warnings.is_empty(),
        "unexpected warnings: {:?}",
        graph.warnings
    );

    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.from == "api.api" && e.to == "migrate.api.api" && e.kind == "owns"),
        "the owning service must wait for its task"
    );
    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.from == "migrate.api.api" && e.to == "db.api.api" && e.kind == "after"),
        "the task must wait for the sibling it names"
    );

    // The whole point: the start order the run layer derives from these
    // edges is db, then migrate, then api.
    let ids: Vec<String> = graph.nodes.iter().map(|n| n.id.clone()).collect();
    let order = crate::runs::topological_start_order(&ids, &graph.edges);
    let pos = |id: &str| order.iter().position(|o| o == id).unwrap();
    assert!(pos("db.api.api") < pos("migrate.api.api"));
    assert!(pos("migrate.api.api") < pos("api.api"));
}

/// `after` names a *sibling* — a dependency of the same owning service. A
/// name that matches nothing is blocking rather than silently dropped: the
/// author declared an ordering constraint, and starting without it runs the
/// seed at a moment their config says is wrong.
#[test]
fn after_naming_no_sibling_is_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "api",
        "  build:\n\
         \x20   context: .\n\
         \x20 dependencies:\n\
         \x20   - kind: task\n\
         \x20     name: migrate\n\
         \x20     command: [\"rake\", \"db:migrate\"]\n\
         \x20     after: [\"nosuchthing\"]\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();
    let warning = graph
        .warnings
        .iter()
        .find(|w| w.message.contains("nosuchthing"))
        .expect("dangling `after` must be reported");
    assert_eq!(warning.severity, Severity::Blocking);
    assert!(
        !graph
            .edges
            .iter()
            .any(|e| e.kind == "after" && e.to.starts_with("nosuchthing")),
        "an unresolvable `after` must not leave a dangling edge behind"
    );
}

/// A task with no command would re-run the image's own long-running `CMD`
/// and never exit, hanging the run until the health budget expires. CUE
/// rejects it too, but the Rust types are the enforcing boundary.
#[test]
fn a_task_with_no_command_is_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "api",
        "  build:\n\
         \x20   context: .\n\
         \x20 dependencies:\n\
         \x20   - kind: task\n\
         \x20     name: migrate\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();
    let warning = graph
        .warnings
        .iter()
        .find(|w| w.message.contains("no `command`"))
        .expect("a commandless task must be reported");
    assert_eq!(warning.severity, Severity::Blocking);
}

/// An image-less task on a service that itself has no `build` has nothing
/// to run at all.
#[test]
fn a_task_with_neither_image_nor_an_inheritable_build_is_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "api",
        "  dependencies:\n\
         \x20   - kind: task\n\
         \x20     name: migrate\n\
         \x20     command: [\"true\"]\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();
    let warning = graph
        .warnings
        .iter()
        .find(|w| w.message.contains("no `image`"))
        .expect("a task with nothing to run must be reported");
    assert_eq!(warning.severity, Severity::Blocking);
}

/// Two tasks each ordered after the other is a contradiction the author
/// wrote, and `topological_start_order` cannot report it — it appends
/// whatever is left in sorted order so a cyclic config still starts
/// something. So the cycle check has to see `after` edges.
///
/// Advisory, not blocking, following the existing policy for every other
/// cycle: both tasks still run and the owner still waits for both, so the
/// worst case is one of them running against unprepared state and exiting
/// non-zero — which blocks the owner visibly rather than corrupting
/// anything quietly.
#[test]
fn an_after_cycle_between_two_tasks_is_caught() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "api",
        "  build:\n\
         \x20   context: .\n\
         \x20 dependencies:\n\
         \x20   - kind: task\n\
         \x20     name: first\n\
         \x20     command: [\"true\"]\n\
         \x20     after: [\"second\"]\n\
         \x20   - kind: task\n\
         \x20     name: second\n\
         \x20     command: [\"true\"]\n\
         \x20     after: [\"first\"]\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        graph
            .warnings
            .iter()
            .any(|w| w.message.contains("cycle") && w.message.contains("first.api.api")),
        "an `after` cycle must be caught: {:?}",
        graph.warnings
    );
}

/// `run: once` round-trips, and is the opt-in — not the default. A
/// migration that only ran when its config hash changed would be skipped
/// after a `git pull` that added one, since the hash covers the image tag
/// and not its contents (`concepts/AUDIT.md` B13).
#[test]
fn run_policy_round_trips_and_defaults_to_on_start() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "api",
        "  build:\n\
         \x20   context: .\n\
         \x20 dependencies:\n\
         \x20   - kind: task\n\
         \x20     name: seed\n\
         \x20     command: [\"seed\"]\n\
         \x20     run: once\n\
         \x20   - kind: task\n\
         \x20     name: migrate\n\
         \x20     command: [\"migrate\"]\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();
    let policy = |id: &str| {
        graph
            .nodes
            .iter()
            .find(|n| n.id == id)
            .unwrap()
            .run_policy
            .clone()
    };
    assert_eq!(policy("seed.api.api").as_deref(), Some("once"));
    assert_eq!(policy("migrate.api.api").as_deref(), Some("on_start"));
}
