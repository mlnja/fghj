use std::fs;

use super::write_component;
use crate::resolver::*;

#[test]
fn backing_dependency_inherits_repo_branch_and_dirty_from_its_owner() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  dependencies:\n\
         \x20   - kind: backing\n\
         \x20     name: mysql\n\
         \x20     image: mysql:8.0.33\n\
         \x20     ports: [\"3306\"]\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    let service = graph
        .nodes
        .iter()
        .find(|n| n.id == "myservice.myservice")
        .unwrap();
    let backing = graph
        .nodes
        .iter()
        .find(|n| n.id == "mysql.myservice.myservice")
        .unwrap();
    // A backing dependency has no checkout of its own — it's declared
    // inline in the owner's `.fghj.yaml` — so its git identity is
    // whatever the owner's is, never independently derived.
    assert_eq!(backing.repo, service.repo);
    assert_eq!(backing.branch, service.branch);
    assert_eq!(backing.dirty, service.dirty);
    assert!(backing.local_path.is_none());
}

/// `kind: service`'s `services:` list lets one dependency block (one
/// `repo`/`default_branch`) name several services owned by the same
/// target repo — this is what replaces repeating a whole block per
/// service. Depends on `repo-b` declaring two services (`api`, `jobs`)
/// and asserts both get their own node and their own `depends-on` edge
/// from the single dependency block in `repo-a`.
#[test]
fn git_dependency_services_list_targets_multiple_services_in_one_repo() {
    let tmp = tempfile::tempdir().unwrap();

    fs::create_dir_all(tmp.path().join("repo-b")).unwrap();
    fs::write(
        tmp.path().join("repo-b/.fghj.yaml"),
        "version: \"1.0\"\n\
         services:\n\
         \x20 api:\n\
         \x20   build:\n\
         \x20     context: .\n\
         \x20 jobs:\n\
         \x20   build:\n\
         \x20     context: .\n",
    )
    .unwrap();

    fs::create_dir_all(tmp.path().join("repo-a")).unwrap();
    fs::write(
        tmp.path().join("repo-a/.fghj.yaml"),
        "version: \"1.0\"\n\
         services:\n\
         \x20 web:\n\
         \x20   build:\n\
         \x20     context: .\n\
         \x20   dependencies:\n\
         \x20     - kind: service\n\
         \x20       repo: https://example.com/repo-b.git\n\
         \x20       default_branch: main\n\
         \x20       services: [api, jobs]\n",
    )
    .unwrap();

    let graph = resolve_universe(tmp.path()).unwrap();

    assert!(graph.nodes.iter().any(|n| n.id == "api.repo-b"));
    assert!(graph.nodes.iter().any(|n| n.id == "jobs.repo-b"));

    let depends_on: Vec<&str> = graph
        .edges
        .iter()
        .filter(|e| e.from == "web.repo-a" && e.kind == "depends-on")
        .map(|e| e.to.as_str())
        .collect();
    assert_eq!(depends_on.len(), 2);
    assert!(depends_on.contains(&"api.repo-b"));
    assert!(depends_on.contains(&"jobs.repo-b"));
    assert!(graph.warnings.is_empty());
}

/// Omitting `services:` entirely on a `kind: service` dependency still
/// defaults to "the" service when the target repo declares exactly one
/// — unchanged behavior, kept as a regression check against the new
/// list-shaped field.
#[test]
fn git_dependency_without_services_defaults_to_the_sole_service() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(tmp.path(), "repo-b", "");

    fs::create_dir_all(tmp.path().join("repo-a")).unwrap();
    fs::write(
        tmp.path().join("repo-a/.fghj.yaml"),
        "version: \"1.0\"\n\
         services:\n\
         \x20 web:\n\
         \x20   build:\n\
         \x20     context: .\n\
         \x20   dependencies:\n\
         \x20     - kind: service\n\
         \x20       repo: https://example.com/repo-b.git\n\
         \x20       default_branch: main\n",
    )
    .unwrap();

    let graph = resolve_universe(tmp.path()).unwrap();

    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.from == "web.repo-a" && e.to == "repo-b.repo-b")
    );
    assert!(graph.warnings.is_empty());
}

/// Omitting `default_branch` on a `kind: service` dependency whose repo
/// isn't on disk yet must leave the stub node's/edge's `branch` as
/// `None`, not `Some("")` — a bare empty string would later reach
/// `git clone --branch ""` in `downloads::clone_stub_logged` and fail,
/// defeating that function's own `unwrap_or("main")` fallback.
#[test]
fn git_dependency_without_default_branch_leaves_branch_unset_on_a_stub() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(tmp.path(), "repo-a", "");
    fs::write(
        tmp.path().join("repo-a/.fghj.yaml"),
        "version: \"1.0\"\n\
         services:\n\
         \x20 web:\n\
         \x20   build:\n\
         \x20     context: .\n\
         \x20   dependencies:\n\
         \x20     - kind: service\n\
         \x20       repo: https://example.com/repo-b.git\n",
    )
    .unwrap();

    let graph = resolve_universe(tmp.path()).unwrap();

    let stub = graph
        .nodes
        .iter()
        .find(|n| n.id == "repo-b")
        .expect("stub node for not-yet-downloaded repo-b");
    assert_eq!(stub.branch, None);

    let edge = graph
        .edges
        .iter()
        .find(|e| e.from == "web.repo-a" && e.to == "repo-b")
        .expect("depends-on edge to the stub");
    assert_eq!(edge.branch, None);
}

#[test]
fn git_dependency_without_repo_targets_a_sibling_service_in_the_same_repo() {
    let tmp = tempfile::tempdir().unwrap();

    fs::create_dir_all(tmp.path().join("shop-web")).unwrap();
    fs::write(
        tmp.path().join("shop-web/.fghj.yaml"),
        "version: \"1.0\"\n\
         services:\n\
         \x20 vite:\n\
         \x20   build:\n\
         \x20     context: .\n\
         \x20   dependencies:\n\
         \x20     - kind: service\n\
         \x20       services: [php]\n\
         \x20 php:\n\
         \x20   build:\n\
         \x20     context: .\n",
    )
    .unwrap();

    let graph = resolve_universe(tmp.path()).unwrap();

    assert!(graph.nodes.iter().any(|n| n.id == "vite.shop-web"));
    assert!(graph.nodes.iter().any(|n| n.id == "php.shop-web"));
    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.from == "vite.shop-web" && e.to == "php.shop-web" && e.kind == "depends-on")
    );
    assert!(graph.warnings.is_empty());
}

#[test]
fn flow_membership_includes_a_sibling_that_depends_on_the_flow_root() {
    let tmp = tempfile::tempdir().unwrap();

    write_component(tmp.path(), "widget", "");

    fs::create_dir_all(tmp.path().join("shop-web")).unwrap();
    fs::write(
        tmp.path().join("shop-web/.fghj.yaml"),
        "version: \"1.0\"\n\
         services:\n\
         \x20 vite:\n\
         \x20   build:\n\
         \x20     context: .\n\
         \x20   dependencies:\n\
         \x20     - kind: service\n\
         \x20       services: [php]\n\
         \x20 php:\n\
         \x20   build:\n\
         \x20     context: .\n\
         flows:\n\
         \x20 demo:\n\
         \x20   description: demo\n\
         \x20   service: php\n\
         \x20   dependencies:\n\
         \x20     - kind: service\n\
         \x20       repo: https://example.com/widget.git\n",
    )
    .unwrap();

    let graph = resolve_universe(tmp.path()).unwrap();

    let php = graph.nodes.iter().find(|n| n.id == "php.shop-web").unwrap();
    assert!(php.flows.contains(&"demo".to_string()));

    // vite depends ON php (the flow root) rather than the other way
    // around — it must still be pulled into the flow's membership, not
    // left looking unrelated just because its edge points backward.
    let vite = graph
        .nodes
        .iter()
        .find(|n| n.id == "vite.shop-web")
        .unwrap();
    assert!(vite.flows.contains(&"demo".to_string()));

    let widget = graph
        .nodes
        .iter()
        .find(|n| n.id.starts_with("widget."))
        .unwrap();
    assert!(widget.flows.contains(&"demo".to_string()));

    let vite_edge = graph
        .edges
        .iter()
        .find(|e| e.from == "vite.shop-web" && e.to == "php.shop-web")
        .unwrap();
    assert!(vite_edge.flows.contains(&"demo".to_string()));
}

#[test]
fn git_dependency_without_repo_on_itself_warns_instead_of_crashing() {
    let tmp = tempfile::tempdir().unwrap();

    fs::create_dir_all(tmp.path().join("shop-web")).unwrap();
    fs::write(
        tmp.path().join("shop-web/.fghj.yaml"),
        "version: \"1.0\"\n\
         services:\n\
         \x20 vite:\n\
         \x20   build:\n\
         \x20     context: .\n\
         \x20   dependencies:\n\
         \x20     - kind: service\n\
         \x20       services: [vite]\n",
    )
    .unwrap();

    let graph = resolve_universe(tmp.path()).unwrap();

    assert!(!graph.edges.iter().any(|e| e.kind == "depends-on"));
    assert!(graph.warnings.iter().any(|w| w.contains("itself")));
}
