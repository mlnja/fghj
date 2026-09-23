//! The resolved output model the UI and the run layer consume.

use std::collections::BTreeMap;

use super::config::Healthcheck;
use super::port::PortConfig;
use super::volume::VolumeMount;
use serde::Serialize;

#[derive(Debug, Serialize, Clone)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub kind: String, // "service" | "backing" | "flow"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// "run" | "stable" — every node's actual `*.fghj.internal` domain is
    /// derived from `id` + workspace (+ run id) by `runs::start_node`; there
    /// is no CUE-declared domain override for any node kind, so this can
    /// never be bypassed. "run" (the default) folds the run id in so two
    /// runs never collide; "stable" (opt-in via `#Service.domain_scope` /
    /// `#BackingDependency.domain_scope`) drops it, for a node a CUE author
    /// deliberately wants one fixed identity shared across every run — only
    /// one run can own that name from the host at a time. Stub (not-yet-
    /// pulled) nodes are always "run": the real value is unknown until the
    /// repo is actually pulled and its `.fghj.yaml` read.
    pub domain_scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    /// This node's canonical `*.fghj.internal` address for the *default*
    /// run — the same value `runs::start_node` derives when it actually
    /// launches a container for this node under `runs::DEFAULT_RUN_ID`.
    /// Populated as a final pass in `resolve_universe` (not at node
    /// construction time, since it needs the workspace name, only known
    /// once resolution is complete) so the UI can show/link to a node's
    /// expected address before any container is running. A node started
    /// under a *named* run gets a different, run-id-qualified domain (see
    /// `runs::derive_domain`) that this field does not reflect.
    pub domain: String,
    pub downloaded: bool,
    /// Whether the on-disk checkout has uncommitted changes — see
    /// `concepts/branch-ownership-model.md`. Always `false` for stub
    /// (`downloaded: false`), backing, and flow nodes, which have no checkout.
    pub dirty: bool,
    /// names of the flows this node is reachable from, in the full resolved universe
    pub flows: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<NodeBuild>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub ports: BTreeMap<String, PortConfig>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub environment: Vec<String>,
    /// Overrides the image's default `CMD` when non-empty — see `#Service`'s
    /// and `#BackingDependency`'s `command` doc comments in
    /// `schema/*.cue`. Passed straight to `docker::RunOpts::command`.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub command: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub volumes: Vec<VolumeMount>,
    /// Extra literal hostnames this service also answers on (`#Service`
    /// only — the non-wildcarded entries of `schema/component.cue`'s
    /// `#Service.additional_hosts`), routed to its `primary` port by
    /// `runs::start_node`. Always empty for backing and stub nodes.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub additional_hosts: Vec<String>,
    /// Same as `additional_hosts`, but each entry also matches every
    /// subdomain of itself (`#Service` only — the `wildcard: true` entries
    /// of `schema/component.cue`'s `#Service.additional_hosts`), routed to
    /// the same `primary` port by `runs::start_node`. Always empty for
    /// backing and stub nodes.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub wildcard_hosts: Vec<String>,
    /// `.env`-style files to load before `environment` — see
    /// `#RunOptions.env_file`'s doc comment. Resolved and merged into
    /// `environment` by `runs::start_node`, not here — resolving a relative
    /// path needs a checkout root (this node's own for a service, the owning
    /// service's for a backing dependency), which isn't known until
    /// `start_node` runs.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub env_file: Vec<String>,
    /// Compose-equivalent restart policy — see `#RunOptions.restart`.
    #[serde(default = "default_restart")]
    pub restart: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub working_dir: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub labels: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub cap_add: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub cap_drop: Vec<String>,
    #[serde(default)]
    pub privileged: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub extra_hosts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub healthcheck: Option<Healthcheck>,
    /// Pins the platform — see `#RunOptions.platform`'s doc comment. For a
    /// service, wired into the build step (`docker::build_image`); for a
    /// backing dependency, into `create_container`'s platform-aware image
    /// lookup. Always `None` for stub nodes.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub platform: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct NodeBuild {
    pub context: String,
    pub dockerfile: String,
    pub args: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String, // "depends-on" | "owns" | "shared-backing"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub flows: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct Graph {
    pub workspace_name: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub warnings: Vec<String>,
}
