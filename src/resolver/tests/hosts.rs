use super::write_component;
use crate::resolver::*;

#[test]
fn additional_hosts_round_trip_into_graph_node() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  ports:\n\
         \x20   \"8080\":\n\
         \x20     primary: true\n\
         \x20 additional_hosts:\n\
         \x20   - aikido.local\n\
         \x20   - demo.example.com\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    let node = graph
        .nodes
        .iter()
        .find(|n| n.id == "myservice.myservice")
        .unwrap();
    assert_eq!(
        node.additional_hosts,
        vec!["aikido.local", "demo.example.com"]
    );
    assert!(graph.warnings.is_empty());
}

#[test]
fn wildcard_hosts_round_trip_into_graph_node() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  ports:\n\
         \x20   \"8080\":\n\
         \x20     primary: true\n\
         \x20 additional_hosts:\n\
         \x20   - host: myservice.local\n\
         \x20     wildcard: true\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    let node = graph
        .nodes
        .iter()
        .find(|n| n.id == "myservice.myservice")
        .unwrap();
    assert_eq!(node.wildcard_hosts, vec!["myservice.local"]);
    assert!(node.additional_hosts.is_empty());
    assert!(graph.warnings.is_empty());
}

#[test]
fn warns_when_wildcard_hosts_declared_without_a_primary_port() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  additional_hosts:\n\
         \x20   - host: myservice.local\n\
         \x20     wildcard: true\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    assert!(
        graph
            .warnings
            .iter()
            .any(|w| w.contains("myservice") && w.contains("additional_hosts"))
    );
}

#[test]
fn warns_when_two_nodes_declare_the_same_wildcard_hosts_suffix() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "service-a",
        "  ports:\n\
         \x20   \"8080\":\n\
         \x20     primary: true\n\
         \x20 additional_hosts:\n\
         \x20   - host: shared.local\n\
         \x20     wildcard: true\n",
    );
    write_component(
        tmp.path(),
        "service-b",
        "  ports:\n\
         \x20   \"8080\":\n\
         \x20     primary: true\n\
         \x20 additional_hosts:\n\
         \x20   - host: shared.local\n\
         \x20     wildcard: true\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    assert!(
        graph
            .warnings
            .iter()
            .any(|w| w.contains("shared.local") && w.contains("more than one"))
    );
}

#[test]
fn warns_when_additional_hosts_declared_without_a_primary_port() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  additional_hosts:\n\
         \x20   - aikido.local\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    assert!(
        graph
            .warnings
            .iter()
            .any(|w| w.contains("myservice") && w.contains("additional_hosts"))
    );
}
