use super::write_component;
use crate::resolver::*;

#[test]
fn ports_round_trip_into_graph_nodes() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  ports:\n\
         \x20   \"8080\":\n\
         \x20     primary: true\n\
         \x20   \"9090\":\n\
         \x20     name: metrics\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    let node = graph
        .nodes
        .iter()
        .find(|n| n.id == "myservice.myservice")
        .unwrap();
    assert_eq!(node.ports.len(), 2);
    assert!(node.ports["8080"].primary);
    assert_eq!(node.ports["9090"].name.as_deref(), Some("metrics"));
    assert!(graph.warnings.is_empty());
}

#[test]
fn warns_when_more_than_one_port_is_primary() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  ports:\n\
         \x20   \"8080\":\n\
         \x20     primary: true\n\
         \x20   \"9090\":\n\
         \x20     primary: true\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    assert!(
        graph
            .warnings
            .iter()
            .any(|w| w.message.contains("myservice")
                && w.message.contains("8080")
                && w.message.contains("9090"))
    );
}

#[test]
fn backing_dependency_ports_accepts_bare_list_or_port_map() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  dependencies:\n\
         \x20   - kind: backing\n\
         \x20     name: postgres\n\
         \x20     image: postgres:16\n\
         \x20     ports: [\"5432\"]\n\
         \x20   - kind: backing\n\
         \x20     name: minio\n\
         \x20     image: minio/minio\n\
         \x20     ports:\n\
         \x20       \"9000\":\n\
         \x20         primary: true\n\
         \x20       \"9001\":\n\
         \x20         name: console\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    let postgres = graph
        .nodes
        .iter()
        .find(|n| n.id == "postgres.myservice.myservice")
        .unwrap();
    assert_eq!(postgres.ports.len(), 1);
    assert!(!postgres.ports["5432"].primary);
    assert!(postgres.ports["5432"].name.is_none());

    let minio = graph
        .nodes
        .iter()
        .find(|n| n.id == "minio.myservice.myservice")
        .unwrap();
    assert_eq!(minio.ports.len(), 2);
    assert!(minio.ports["9000"].primary);
    assert_eq!(minio.ports["9001"].name.as_deref(), Some("console"));
    assert!(graph.warnings.is_empty());
}

#[test]
fn warns_when_backing_dependency_declares_more_than_one_primary_port() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  dependencies:\n\
         \x20   - kind: backing\n\
         \x20     name: grafana\n\
         \x20     image: grafana/grafana\n\
         \x20     ports:\n\
         \x20       \"3000\":\n\
         \x20         primary: true\n\
         \x20       \"3100\":\n\
         \x20         primary: true\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    assert!(graph.warnings.iter().any(|w| w.message.contains("grafana")
        && w.message.contains("3000")
        && w.message.contains("3100")));
}

#[test]
fn port_wildcard_round_trips_into_node_ports() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  ports:\n\
         \x20   \"8080\":\n\
         \x20     primary: true\n\
         \x20     wildcard: true\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    let node = graph
        .nodes
        .iter()
        .find(|n| n.id == "myservice.myservice")
        .unwrap();
    assert!(node.ports["8080"].wildcard);
    assert!(graph.warnings.is_empty());
}

#[test]
fn warns_when_port_wildcard_set_without_primary_or_name() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  ports:\n\
         \x20   \"8080\":\n\
         \x20     wildcard: true\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    assert!(
        graph
            .warnings
            .iter()
            .any(|w| w.message.contains("myservice") && w.message.contains("wildcard"))
    );
}
