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
