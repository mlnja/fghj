use super::write_component;
use crate::resolver::*;

#[test]
fn service_domain_scope_defaults_to_run_and_can_opt_into_stable() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(tmp.path(), "svc-a", "");
    write_component(tmp.path(), "svc-b", "  domain_scope: stable\n");

    let graph = resolve_universe(tmp.path()).unwrap();

    let a = graph.nodes.iter().find(|n| n.id == "svc-a.svc-a").unwrap();
    let b = graph.nodes.iter().find(|n| n.id == "svc-b.svc-b").unwrap();
    assert_eq!(a.domain_scope, "run");
    assert_eq!(b.domain_scope, "stable");
}
