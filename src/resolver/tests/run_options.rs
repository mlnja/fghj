use super::{write_component, write_yaml};
use crate::resolver::*;

#[test]
fn command_round_trips_into_graph_node_for_service_and_backing() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "myservice",
        r#"version: "2.0"
services:
  myservice:
    build: .
    command: ["npm", "run", "dev"]
  mysql:
    image: mysql:8.0.33
    ports: ["3306"]
    command: ["mysqld", "--sql_mode=NO_ENGINE_SUBSTITUTION"]
"#,
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    let service = graph
        .nodes
        .iter()
        .find(|n| n.id == "myservice.myservice")
        .unwrap();
    assert_eq!(service.command, vec!["npm", "run", "dev"]);

    let backing = graph
        .nodes
        .iter()
        .find(|n| n.id == "mysql.myservice")
        .unwrap();
    assert_eq!(
        backing.command,
        vec!["mysqld", "--sql_mode=NO_ENGINE_SUBSTITUTION"]
    );
}

#[test]
fn run_options_round_trip_into_graph_node_for_service_and_backing() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "myservice",
        r#"version: "2.0"
services:
  myservice:
    build: .
    env_file:
      - .env
    platform: linux/arm64
    restart: always
    user: "1000:1000"
    working_dir: /app
    labels:
      team: platform
    cap_add: ["NET_ADMIN"]
    cap_drop: ["ALL"]
    privileged: true
    extra_hosts:
      - "metadata:169.254.169.254"
    stop_signal: SIGQUIT
    stop_grace_period: 30
  postgres:
    image: postgres:16
    ports: ["5432"]
    platform: linux/amd64
    env_file:
      - .env.postgres
    restart: unless-stopped
    stop_grace_period: 120
    healthcheck:
      test: ["CMD", "pg_isready"]
      interval: 5
      retries: 3
"#,
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    let service = graph
        .nodes
        .iter()
        .find(|n| n.id == "myservice.myservice")
        .unwrap();
    assert_eq!(service.env_file, vec![".env"]);
    assert_eq!(service.restart, "always");
    assert_eq!(service.user.as_deref(), Some("1000:1000"));
    assert_eq!(service.working_dir.as_deref(), Some("/app"));
    assert_eq!(
        service.labels.get("team").map(String::as_str),
        Some("platform")
    );
    assert_eq!(service.cap_add, vec!["NET_ADMIN"]);
    assert_eq!(service.cap_drop, vec!["ALL"]);
    assert!(service.privileged);
    assert_eq!(service.extra_hosts, vec!["metadata:169.254.169.254"]);
    assert_eq!(service.platform.as_deref(), Some("linux/arm64"));
    assert_eq!(service.stop_signal.as_deref(), Some("SIGQUIT"));
    assert_eq!(service.stop_grace_period, 30);

    let backing = graph
        .nodes
        .iter()
        .find(|n| n.id == "postgres.myservice")
        .unwrap();
    assert_eq!(backing.platform.as_deref(), Some("linux/amd64"));
    assert_eq!(backing.env_file, vec![".env.postgres"]);
    assert_eq!(backing.restart, "unless-stopped");
    // The case this knob exists for: a database that needs longer than
    // Docker's 10s to flush before it is killed.
    assert!(backing.stop_signal.is_none());
    assert_eq!(backing.stop_grace_period, 120);
    let hc = backing.healthcheck.as_ref().unwrap();
    assert_eq!(hc.test, vec!["CMD", "pg_isready"]);
    assert_eq!(hc.interval, Some(5));
    assert_eq!(hc.retries, Some(3));
}

#[test]
fn run_options_default_to_compose_equivalent_no_ops() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(tmp.path(), "myservice", "");

    let graph = resolve_universe(tmp.path()).unwrap();
    let service = graph
        .nodes
        .iter()
        .find(|n| n.id == "myservice.myservice")
        .unwrap();
    assert_eq!(service.restart, "no");
    assert!(service.user.is_none());
    assert!(service.labels.is_empty());
    assert!(!service.privileged);
    assert!(service.healthcheck.is_none());
    // Docker's own default grace period, and no signal override — so a
    // config that says nothing about stopping behaves exactly as it did
    // before these fields existed.
    assert!(service.stop_signal.is_none());
    assert_eq!(service.stop_grace_period, 10);
}

/// A task has no `restart` and no `healthcheck`, but it does get the stop
/// knobs — a migration caught mid-run by a teardown is precisely the thing
/// that should not be SIGKILLed.
#[test]
fn a_task_carries_the_stop_knobs_even_though_it_has_no_restart_policy() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "myservice",
        r#"version: "2.0"
services:
  myservice:
    build: .
    depends_on:
      migrate: {condition: service_completed_successfully}
  migrate:
    build: .
    command: ["rake", "db:migrate"]
    stop_signal: SIGINT
    stop_grace_period: 300
"#,
    );

    let graph = resolve_universe(tmp.path()).unwrap();
    let task = graph
        .nodes
        .iter()
        .find(|n| n.id == "migrate.myservice")
        .unwrap();
    assert_eq!(task.kind, "task");
    assert_eq!(task.stop_signal.as_deref(), Some("SIGINT"));
    assert_eq!(task.stop_grace_period, 300);
    assert_eq!(task.restart, "no");
}

/// CUE's `stop_signal` pattern and the Rust check must agree, so an author
/// who writes the unprefixed name Docker itself would have taken gets one
/// clear answer rather than two different ones.
#[test]
fn an_unprefixed_stop_signal_is_a_blocking_warning() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(tmp.path(), "myservice", "  stop_signal: TERM\n");

    let graph = resolve_universe(tmp.path()).unwrap();
    let warning = graph
        .warnings
        .iter()
        .find(|w| w.message.contains("stop_signal"))
        .expect("an unprefixed signal name earns a warning");
    assert_eq!(warning.severity, Severity::Blocking);
    assert!(warning.message.contains("SIGTERM"));
}

#[test]
fn a_well_formed_stop_signal_earns_no_warning() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(tmp.path(), "myservice", "  stop_signal: SIGQUIT\n");

    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        !graph
            .warnings
            .iter()
            .any(|w| w.message.contains("stop_signal"))
    );
}

/// `#Healthcheck.test` is `[_, ...]` in CUE — non-empty. The Rust side is the
/// enforcing boundary, so if it silently accepted `test: []` the schema would
/// be rejecting a file the daemon runs, which is the one asymmetry the two are
/// not allowed to have. Blocking, because the failure mode is silent: Docker
/// reads an empty `Test` as "inherit the image's healthcheck", so the node
/// ends up with no healthcheck at all — and the run-wide health budget then
/// waits on nothing.
#[test]
fn warns_when_a_healthcheck_declares_an_empty_test() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  healthcheck:\n\
         \x20   test: []\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    let w = graph
        .warnings
        .iter()
        .find(|w| w.message.contains("empty `test`"))
        .expect("an empty healthcheck test must be reported");
    assert!(matches!(w.severity, Severity::Blocking));
    assert!(w.message.contains("myservice.myservice"));
}

/// The same check applies to a backing service's healthcheck, which is the
/// far more common place to write one (a `pg_isready` on a database).
#[test]
fn warns_when_a_backing_healthcheck_declares_an_empty_test() {
    let tmp = tempfile::tempdir().unwrap();
    write_yaml(
        tmp.path(),
        "myservice",
        r#"version: "2.0"
services:
  db:
    image: postgres:16
    healthcheck:
      test: []
"#,
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    assert!(
        graph
            .warnings
            .iter()
            .any(|w| w.message.contains("empty `test`") && w.message.contains("db.myservice"))
    );
}
