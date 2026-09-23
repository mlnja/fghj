use std::collections::BTreeMap;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::resolver::{Healthcheck, Node};

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
        user: Option<&'a str>,
        working_dir: Option<&'a str>,
        labels: &'a BTreeMap<String, String>,
        cap_add: &'a [String],
        cap_drop: &'a [String],
        privileged: bool,
        extra_hosts: &'a [String],
        healthcheck: Option<&'a Healthcheck>,
        platform: Option<&'a str>,
    }

    let desired = DesiredSpec {
        image: &spec.image,
        command: &node.command,
        env: &spec.env,
        ports: &spec.port_list,
        binds: &spec.binds,
        restart: &node.restart,
        user: node.user.as_deref(),
        working_dir: node.working_dir.as_deref(),
        labels: &node.labels,
        cap_add: &node.cap_add,
        cap_drop: &node.cap_drop,
        privileged: node.privileged,
        extra_hosts: &node.extra_hosts,
        healthcheck: node.healthcheck.as_ref(),
        platform: node.platform.as_deref(),
    };
    let bytes = serde_json::to_vec(&desired).expect("DesiredSpec always serializes");
    let digest = Sha256::digest(&bytes);
    format!("{digest:x}")
}
