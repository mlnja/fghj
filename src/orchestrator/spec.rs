use std::collections::BTreeMap;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::resolver::{Healthcheck, Node, NodeBuild};

pub(crate) struct NodeSpec {
    pub(crate) container_name: String,
    pub(crate) domain: String,
    pub(crate) raw_domain: String,
    pub(crate) aliases: Vec<String>,
    pub(crate) image: String,
    pub(crate) port_list: Vec<(String, Option<u16>)>,
    pub(crate) binds: Vec<String>,
    pub(crate) env: Vec<String>,
}

/// Hashes what affects how the container runs. Excludes identity (derived from
/// `node.id`) and Docker-chosen host ports. Sha256 because `DefaultHasher`
/// isn't stable across Rust versions.
pub(crate) fn spec_hash(node: &Node, spec: &NodeSpec) -> String {
    #[derive(Serialize)]
    struct DesiredSpec<'a> {
        image: &'a str,
        command: &'a [String],
        env: &'a [String],
        ports: &'a [(String, Option<u16>)],
        binds: &'a [String],
        restart: &'a str,
        stop_signal: Option<&'a str>,
        stop_grace_period: u64,
        user: Option<&'a str>,
        working_dir: Option<&'a str>,
        labels: &'a BTreeMap<String, String>,
        cap_add: &'a [String],
        cap_drop: &'a [String],
        privileged: bool,
        extra_hosts: &'a [String],
        healthcheck: Option<&'a Healthcheck>,
        platform: Option<&'a str>,
        /// The tag is stable across rebuilds, so a changed `args` or `target` would
        /// otherwise be invisible.
        build: Option<&'a NodeBuild>,
        /// The checkout the image is built from (the clone, for a git context); `None`
        /// for a published image. The tag doesn't change per commit, so this is what
        /// makes a new commit visible.
        source: Option<SourceState<'a>>,
    }

    /// `dirty` is one bit: it catches clean → dirty, not further edits. Restart
    /// rebuilds unconditionally for that.
    #[derive(Serialize)]
    struct SourceState<'a> {
        head: Option<&'a str>,
        dirty: bool,
    }

    let desired = DesiredSpec {
        image: &spec.image,
        command: &node.command,
        env: &spec.env,
        ports: &spec.port_list,
        binds: &spec.binds,
        restart: &node.restart,
        stop_signal: node.stop_signal.as_deref(),
        stop_grace_period: node.stop_grace_period,
        user: node.user.as_deref(),
        working_dir: node.working_dir.as_deref(),
        labels: &node.labels,
        cap_add: &node.cap_add,
        cap_drop: &node.cap_drop,
        privileged: node.privileged,
        extra_hosts: &node.extra_hosts,
        healthcheck: node.healthcheck.as_ref(),
        platform: node.platform.as_deref(),
        build: node.build.as_ref(),
        source: node.build_checkout().map(|c| SourceState {
            head: c.head,
            dirty: c.dirty,
        }),
    };
    let bytes = serde_json::to_vec(&desired).expect("DesiredSpec always serializes");
    let digest = Sha256::digest(&bytes);
    format!("{digest:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::testing::test_node;
    use crate::resolver::NodeBuild;

    fn empty_spec() -> NodeSpec {
        NodeSpec {
            container_name: "c".to_string(),
            domain: "d".to_string(),
            raw_domain: "d".to_string(),
            aliases: Vec::new(),
            image: "fghj/api:main".to_string(),
            port_list: Vec::new(),
            binds: Vec::new(),
            env: Vec::new(),
        }
    }

    fn build() -> NodeBuild {
        NodeBuild {
            context: ".".to_string(),
            dockerfile: "Dockerfile".to_string(),
            dockerfile_inline: None,
            source: None,
            args: BTreeMap::new(),
            target: None,
            ssh: false,
            secrets: Vec::new(),
        }
    }

    #[test]
    fn changing_a_build_arg_changes_the_hash_even_though_the_tag_does_not() {
        let mut node = test_node("api.api", "api", "service");
        node.build = Some(build());
        let before = spec_hash(&node, &empty_spec());

        let mut changed = build();
        changed
            .args
            .insert("RUST_VERSION".to_string(), "1.94".to_string());
        node.build = Some(changed);

        assert_ne!(before, spec_hash(&node, &empty_spec()));
    }

    #[test]
    fn changing_the_build_target_changes_the_hash() {
        let mut node = test_node("api.api", "api", "service");
        node.build = Some(build());
        let before = spec_hash(&node, &empty_spec());

        let mut changed = build();
        changed.target = Some("builder".to_string());
        node.build = Some(changed);

        assert_ne!(before, spec_hash(&node, &empty_spec()));
    }

    #[test]
    fn a_new_commit_on_the_same_branch_changes_the_hash() {
        let mut node = test_node("api.api", "api", "service");
        node.build = Some(build());
        node.head = Some("a".repeat(40));
        let before = spec_hash(&node, &empty_spec());

        node.head = Some("b".repeat(40));
        assert_ne!(before, spec_hash(&node, &empty_spec()));
    }

    #[test]
    fn dirtying_the_checkout_changes_the_hash() {
        let mut node = test_node("api.api", "api", "service");
        node.build = Some(build());
        node.head = Some("a".repeat(40));
        let before = spec_hash(&node, &empty_spec());

        node.dirty = true;
        assert_ne!(before, spec_hash(&node, &empty_spec()));
    }

    fn sourced(head: &str) -> NodeBuild {
        NodeBuild {
            source: Some(crate::resolver::BuildSource {
                url: "https://example.com/geocoder.git".to_string(),
                reference: Some("v1".to_string()),
                path: ".fghj/sources/geocoder@v1".to_string(),
                downloaded: true,
                head: Some(head.to_string()),
                dirty: false,
            }),
            ..build()
        }
    }

    #[test]
    fn a_git_build_context_drifts_with_its_clone_not_the_declaring_repo() {
        let mut node = test_node("geocoder.api", "geocoder", "service");
        node.build = Some(sourced(&"a".repeat(40)));
        node.head = Some("1".repeat(40));
        let before = spec_hash(&node, &empty_spec());

        node.head = Some("2".repeat(40));
        assert_eq!(before, spec_hash(&node, &empty_spec()));

        node.build = Some(sourced(&"b".repeat(40)));
        assert_ne!(before, spec_hash(&node, &empty_spec()));
    }

    #[test]
    fn a_commit_does_not_drift_a_node_that_runs_a_published_image() {
        let mut node = test_node("db.api", "db", "backing");
        node.build = None;
        node.head = Some("a".repeat(40));
        let before = spec_hash(&node, &empty_spec());

        node.head = Some("b".repeat(40));
        node.dirty = true;
        assert_eq!(before, spec_hash(&node, &empty_spec()));
    }

    #[test]
    fn changing_a_stop_knob_changes_the_hash() {
        let node = test_node("db.api", "db", "backing");
        let before = spec_hash(&node, &empty_spec());

        let mut slower = test_node("db.api", "db", "backing");
        slower.stop_grace_period = 120;
        assert_ne!(before, spec_hash(&slower, &empty_spec()));

        let mut signalled = test_node("db.api", "db", "backing");
        signalled.stop_signal = Some("SIGQUIT".to_string());
        assert_ne!(before, spec_hash(&signalled, &empty_spec()));
    }

    #[test]
    fn an_unchanged_build_hashes_identically() {
        let mut node = test_node("api.api", "api", "service");
        node.build = Some(build());

        assert_eq!(
            spec_hash(&node, &empty_spec()),
            spec_hash(&node, &empty_spec())
        );
    }
}
