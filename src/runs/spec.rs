use std::collections::BTreeMap;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::resolver::{Healthcheck, Node, NodeBuild};

/// The pure, side-effect-free result of `RunRegistry::resolve_node_spec` —
/// everything about a node's config that has to be actually computed (as
/// opposed to read straight off `Node`) before it can be either run for real
/// (`start_node`) or hashed to check for drift (`spec_hash`,
/// `config_drift`).
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

/// Hashes everything about `node` + `spec` that actually affects how the
/// container runs, for `ContainerInfo::config_hash` /
/// `RunRegistry::config_drift` to compare against. Deliberately
/// excludes anything that's either pure identity (`container_name`, `domain`,
/// `aliases` all derive one-to-one from `node.id` + run id — never drift
/// independently of the rest of the spec) or genuinely ephemeral (an
/// unpinned port's actual host-side binding is chosen fresh by Docker on
/// every start; hashing it would flag every single restart as "drifted").
/// Uses `Sha256` rather than `std::hash::DefaultHasher`, which is explicitly
/// documented as unstable across Rust versions — that would misreport a
/// clean upgrade of the `fghj` binary itself as config drift.
pub(crate) fn spec_hash(node: &Node, spec: &NodeSpec) -> String {
    #[derive(Serialize)]
    struct DesiredSpec<'a> {
        image: &'a str,
        command: &'a [String],
        env: &'a [String],
        ports: &'a [(String, Option<u16>)],
        binds: &'a [String],
        restart: &'a str,
        /// Both stop knobs are hashed: they are baked into the container at
        /// create time, so changing either in `.fghj.yaml` has no effect
        /// until the container is recreated — which is exactly what drift
        /// means.
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
        /// The `#Build` block, for a node whose image fghj builds itself.
        ///
        /// Included even though the built image's *tag* is already in
        /// `image` above, because the tag is stable across rebuilds
        /// (`fghj/<id>:<branch>`): without this, editing `args` or `target`
        /// changed what the next build produces while leaving the hash
        /// identical, so nothing downstream ever noticed. This does not
        /// make the hash content-aware — see `source` below for how much of
        /// that gap is covered and how.
        build: Option<&'a NodeBuild>,
        /// The state of the checkout the image is built from — `None` for a
        /// node running a published `image:`, where a commit in the repo that
        /// happens to declare it says nothing about that container.
        ///
        /// This is what makes a new commit visible. The tag fghj builds is
        /// `fghj/{id}:{branch}`, stable across commits, so before this you
        /// could commit, pull or rebase on the same branch and the container
        /// stayed lit as `Synced` while serving a stale image — a branch
        /// *switch* was caught only incidentally, because the tag changed.
        ///
        /// For a git build context it is the clone's state, not the
        /// declaring repo's: the code is built from the clone, and a commit
        /// in the declaring repo that changes the definition shows up in the
        /// rest of the hash anyway.
        source: Option<SourceState<'a>>,
    }

    /// See `DesiredSpec::source`. Two fields, with quite different
    /// resolutions.
    ///
    /// `head` is exact: every commit, pull, rebase or branch switch moves it.
    ///
    /// `dirty` is one bit, and so catches only the clean → dirty transition.
    /// Edit a file in a clean checkout and the node reads drifted; edit
    /// another file in an already-dirty checkout and it does not. That is a
    /// real gap and it is the honest one to leave: closing it means hashing
    /// build-context contents on every drift tick, and the tool for "I am
    /// iterating on this file right now" is the per-node Restart button,
    /// which rebuilds unconditionally — not a drift pill.
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
    use crate::resolver::NodeBuild;
    use crate::runs::testing::test_node;

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

    /// The whole point of putting `build` in the hash: the image *tag* is
    /// stable across rebuilds, so before this every `#Build` edit produced an
    /// identical hash and drift detection saw nothing at all.
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

    /// [B13]: the tag is `fghj/<id>:<branch>`, so a commit on the same
    /// branch left the hash byte-identical while the container served a stale
    /// image.
    #[test]
    fn a_new_commit_on_the_same_branch_changes_the_hash() {
        let mut node = test_node("api.api", "api", "service");
        node.build = Some(build());
        node.head = Some("a".repeat(40));
        let before = spec_hash(&node, &empty_spec());

        node.head = Some("b".repeat(40));
        assert_ne!(before, spec_hash(&node, &empty_spec()));
    }

    /// Dirtying a clean checkout is a change to what the next build would
    /// produce, so it counts.
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

    /// A git build context is built from its clone, so the clone's HEAD is
    /// what drifts it — and a commit in the repo that merely declares it
    /// doesn't.
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

    /// A node running a published image must *not* move when the repo that
    /// declares it gets a commit — otherwise every `postgres:16` in the
    /// workspace would read as drifted on every commit, which is a signal
    /// nobody would read.
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

    /// The stop knobs are baked into the container by `create_container`, so
    /// editing them in `.fghj.yaml` does nothing at all until the container
    /// is recreated. That is drift by definition, and it only shows up if
    /// they are in the hash.
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

    /// Hashing has to stay a pure function of the config — a node whose
    /// `#Build` is untouched must hash the same on every call, or every
    /// reconcile loop would read as drift.
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
