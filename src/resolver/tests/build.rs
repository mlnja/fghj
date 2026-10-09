use super::write_component;
use crate::resolver::*;

#[test]
fn build_inputs_round_trip_into_the_graph_node() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "myservice",
        "  build:\n\
         \x20   context: ./docker\n\
         \x20   dockerfile: Dockerfile.dev\n\
         \x20   target: builder\n\
         \x20   ssh: true\n\
         \x20   args:\n\
         \x20     RUST_VERSION: \"1.94\"\n\
         \x20   secrets:\n\
         \x20     - id: npmrc\n\
         \x20       file: .npmrc\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    let build = graph
        .nodes
        .iter()
        .find(|n| n.id == "myservice.myservice")
        .unwrap()
        .build
        .as_ref()
        .expect("a service with a build block keeps it");
    assert_eq!(build.context, "./docker");
    assert_eq!(build.dockerfile, "Dockerfile.dev");
    assert_eq!(build.target.as_deref(), Some("builder"));
    assert!(build.ssh);
    assert_eq!(build.args.get("RUST_VERSION").unwrap(), "1.94");
    assert_eq!(build.secrets.len(), 1);
    assert_eq!(build.secrets[0].id, "npmrc");
    assert_eq!(build.secrets[0].file, ".npmrc");
}

/// Every field added by `concepts/build-inputs.md` is optional, so a config
/// written before they existed has to keep resolving to the same build it
/// always did — no target, no forwarded agent, no secrets.
#[test]
fn a_build_block_without_the_new_fields_keeps_its_old_meaning() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(tmp.path(), "myservice", "  build:\n\x20   context: .\n");

    let graph = resolve_universe(tmp.path()).unwrap();

    let build = graph
        .nodes
        .iter()
        .find(|n| n.id == "myservice.myservice")
        .unwrap()
        .build
        .as_ref()
        .unwrap();
    assert_eq!(build.dockerfile, "Dockerfile");
    assert_eq!(build.target, None);
    assert!(!build.ssh);
    assert!(build.secrets.is_empty());
}

fn build_of(graph: &Graph, id: &str) -> NodeBuild {
    graph
        .nodes
        .iter()
        .find(|n| n.id == id)
        .unwrap()
        .build
        .clone()
        .unwrap()
}

/// `concepts/git-build-sources.md`: the code comes from the URL, the service
/// is still this repo's.
#[test]
fn a_git_context_builds_this_repos_service_from_a_clone() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "shop",
        "  build:\n\
         \x20   context: https://github.com/acme/geocoder.git#v2.3.1:server\n\
         \x20   dockerfile_inline: |\n\
         \x20     FROM scratch\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();

    let build = build_of(&graph, "shop.shop");
    assert_eq!(build.context, "server");
    assert_eq!(build.dockerfile_inline.as_deref(), Some("FROM scratch\n"));
    let source = build.source.unwrap();
    assert_eq!(source.url, "https://github.com/acme/geocoder.git");
    assert_eq!(source.reference.as_deref(), Some("v2.3.1"));
    assert_eq!(source.path, ".fghj/sources/geocoder@v2.3.1");
    assert!(!source.downloaded);
    assert!(graph.warnings.iter().all(|w| !w.is_blocking()));
}

#[test]
fn a_local_context_has_no_source() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(tmp.path(), "shop", "  build: ./docker\n");

    let build = build_of(&resolve_universe(tmp.path()).unwrap(), "shop.shop");
    assert_eq!(build.context, "./docker");
    assert!(build.source.is_none());
}

#[test]
fn both_dockerfile_and_dockerfile_inline_is_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "shop",
        "  build:\n\
         \x20   dockerfile: Dockerfile.dev\n\
         \x20   dockerfile_inline: FROM scratch\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(graph.warnings.iter().any(|w| {
        w.is_blocking()
            && w.message
                .contains("both `dockerfile` and `dockerfile_inline`")
    }));
}

#[test]
fn a_subdir_outside_the_clone_is_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "shop",
        "  build: https://example.com/geocoder.git#main:../../etc\n",
    );

    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(
        graph
            .warnings
            .iter()
            .any(|w| w.is_blocking() && w.message.contains("outside the clone"))
    );
}

/// Same URL and ref share one clone; two repos named alike would fight over
/// one folder.
#[test]
fn two_repos_wanting_the_same_clone_folder_is_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    write_component(
        tmp.path(),
        "shop",
        "  build: https://github.com/acme/geocoder.git#main\n",
    );
    write_component(
        tmp.path(),
        "maps",
        "  build: https://github.com/acme/geocoder.git#main\n",
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(graph.warnings.iter().all(|w| !w.is_blocking()));

    write_component(
        tmp.path(),
        "maps",
        "  build: https://github.com/fork/geocoder.git#main\n",
    );
    let graph = resolve_universe(tmp.path()).unwrap();
    assert!(graph.warnings.iter().any(|w| w.is_blocking()
        && w.message.contains(".fghj/sources/geocoder@main")
        && w.message.contains("maps.maps, shop.shop")));
}
