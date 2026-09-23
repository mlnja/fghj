use super::write_component;
use crate::resolver::*;

#[test]
fn service_bind_mount_round_trips_into_graph_node() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  volumes:\n\
         \x20   - host: ./src\n\
         \x20     container: /app/src\n\
         \x20   - host: ../intel\n\
         \x20     container: /app/intel\n\
         \x20     read_only: true\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    let node = graph
        .nodes
        .iter()
        .find(|n| n.id == "myservice.myservice")
        .unwrap();
    assert_eq!(node.volumes.len(), 2);
    assert!(matches!(
        &node.volumes[0],
        VolumeMount::Bind { host, container, read_only }
            if host == "./src" && container == "/app/src" && !read_only
    ));
    assert!(matches!(
        &node.volumes[1],
        VolumeMount::Bind { host, container, read_only }
            if host == "../intel" && container == "/app/intel" && *read_only
    ));
}

#[test]
fn named_volume_round_trips_into_graph_node() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  dependencies:\n\
         \x20   - kind: backing\n\
         \x20     name: postgres\n\
         \x20     image: postgres:16\n\
         \x20     ports: [\"5432\"]\n\
         \x20     volumes:\n\
         \x20       - name: pgdata\n\
         \x20         container: /var/lib/postgresql/data\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    let node = graph
        .nodes
        .iter()
        .find(|n| n.id == "postgres.myservice.myservice")
        .unwrap();
    assert_eq!(node.volumes.len(), 1);
    assert!(matches!(
        &node.volumes[0],
        VolumeMount::Named { name, scope, container, read_only }
            if name == "pgdata" && scope == "run" && container == "/var/lib/postgresql/data" && !read_only
    ));
}
