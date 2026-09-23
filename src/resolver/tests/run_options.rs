use super::write_component;
use crate::resolver::*;

#[test]
fn command_round_trips_into_graph_node_for_service_and_backing() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  command: [\"npm\", \"run\", \"dev\"]\n\
         \x20 dependencies:\n\
         \x20   - kind: backing\n\
         \x20     name: mysql\n\
         \x20     image: mysql:8.0.33\n\
         \x20     ports: [\"3306\"]\n\
         \x20     command: [\"mysqld\", \"--sql_mode=NO_ENGINE_SUBSTITUTION\"]\n",
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
        .find(|n| n.id == "mysql.myservice.myservice")
        .unwrap();
    assert_eq!(
        backing.command,
        vec!["mysqld", "--sql_mode=NO_ENGINE_SUBSTITUTION"]
    );
}

#[test]
fn run_options_round_trip_into_graph_node_for_service_and_backing() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  env_file:\n\
         \x20   - .env\n\
         \x20 platform: linux/arm64\n\
         \x20 restart: always\n\
         \x20 user: \"1000:1000\"\n\
         \x20 working_dir: /app\n\
         \x20 labels:\n\
         \x20   team: platform\n\
         \x20 cap_add: [\"NET_ADMIN\"]\n\
         \x20 cap_drop: [\"ALL\"]\n\
         \x20 privileged: true\n\
         \x20 extra_hosts:\n\
         \x20   - \"metadata:169.254.169.254\"\n\
         \x20 dependencies:\n\
         \x20   - kind: backing\n\
         \x20     name: postgres\n\
         \x20     image: postgres:16\n\
         \x20     ports: [\"5432\"]\n\
         \x20     platform: linux/amd64\n\
         \x20     env_file:\n\
         \x20       - .env.postgres\n\
         \x20     restart: unless-stopped\n\
         \x20     healthcheck:\n\
         \x20       test: [\"CMD\", \"pg_isready\"]\n\
         \x20       interval: 5\n\
         \x20       retries: 3\n",
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

    let backing = graph
        .nodes
        .iter()
        .find(|n| n.id == "postgres.myservice.myservice")
        .unwrap();
    assert_eq!(backing.platform.as_deref(), Some("linux/amd64"));
    assert_eq!(backing.env_file, vec![".env.postgres"]);
    assert_eq!(backing.restart, "unless-stopped");
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
}
