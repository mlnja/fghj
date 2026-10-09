//! The resolved output model the UI and the run layer consume.

use std::collections::{BTreeMap, BTreeSet};

use super::config::Healthcheck;
use super::port::PortConfig;
use super::volume::VolumeMount;
use super::warning::Warning;
use serde::Serialize;

#[derive(Debug, Serialize, Clone)]
pub struct Node {
    pub id: String,
    pub label: String,
    /// "service" | "backing" | "task". A `task` node is the terminating
    /// kind: its container is *supposed* to exit, and exit 0 is success
    /// rather than the drift the same observed status means for the other
    /// two. See `schema/dependency.cue`'s `#Task` and
    /// [[state-and-effects]]-adjacent `state::ContainerDesired::terminating`,
    /// which is how that distinction survives into the run layer.
    pub kind: String,
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
    /// `concepts/branch-ownership-model.md`. `false` for a stub
    /// (`downloaded: false`) and for a flow node, neither of which has a
    /// checkout. A backing dependency or task *does* carry one: it inherits
    /// its owning service's, exactly as it inherits `repo`/`branch`/`head`
    /// (`resolver::visit_dependency`), because the owner's tree is what its
    /// image was built from.
    ///
    /// Note the failure case is `true`, not `false`: `git_status_dirty` ends
    /// in `unwrap_or(true)` because it cannot vouch for a tree it could not
    /// read. A node whose git state is unreadable therefore reports dirty,
    /// and `branch`/`head` are `None` alongside it — which is the pair the UI
    /// keys on to say "unreadable" rather than presenting the `true` as a
    /// finding.
    pub dirty: bool,
    /// The commit the checkout is on, as a full SHA — `None` for a node with
    /// no checkout of its own to read (a stub, a flow) or when the directory
    /// isn't a git working tree. A backing dependency or task inherits its
    /// owning service's, exactly as it inherits `repo`/`branch`/`dirty`.
    ///
    /// Hashed into `spec_hash` **only** for a node whose image fghj builds
    /// itself (`build.is_some()`); that is what makes a new commit on the
    /// same branch visible as drift, since the image tag
    /// `fghj/{id}:{branch}` does not change. A node running a published
    /// `image:` ignores it — a commit in the repo that happens to declare a
    /// `postgres:16` dependency says nothing about that container.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub head: Option<String>,
    /// The flows whose run starts this node, as `{repo}/{flow}` — the
    /// flow's members plus everything they can't start without. See
    /// `concepts/flows-v2.md`.
    pub flows: Vec<String>,
    /// This node's repo's `include:` aliases, resolved to the folder (or
    /// stub id) each points at. What `${FGHJ_SERVICE_FQDN:alias/name}`
    /// looks up — carried on the node because `env_file` values are only
    /// read, and expanded, when the node starts.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub includes: BTreeMap<String, String>,
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
    /// `#RunOptions.stop_signal` — `None` leaves the image's own `STOPSIGNAL`
    /// (SIGTERM unless the image says otherwise).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub stop_signal: Option<String>,
    /// `#RunOptions.stop_grace_period` — seconds Docker waits after the stop
    /// signal before SIGKILL. Both of these are stamped onto the container at
    /// create time rather than passed with each stop call, so the policy
    /// outlives the `Node` that asked for it.
    #[serde(default = "default_stop_grace_period")]
    pub stop_grace_period: u64,
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
    /// `#Task.run` — "on_start" or "once". `None` for every kind that isn't
    /// a task, which is the honest reading: a long-running service has no
    /// re-run policy because it has nothing to re-run.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub run_policy: Option<String>,
    /// `#RunOptions.debug` — the in-container port this node's debugger
    /// listens on, or `None` for a node that declares none.
    ///
    /// Carries into the run layer in exactly one place: `runs::node_spec`
    /// publishes it alongside the node's declared `ports`, so it answers at
    /// `{raw_domain}:{debug}` on the declared number, host and in-network
    /// alike. Nothing is injected into the container's environment, for the
    /// same reason nothing is injected for `ports`. fghj
    /// never learns which debugger it is — see
    /// `concepts/debugging-in-containers.md` for why that stays the image's
    /// business, and `state::ContainerDesired::debug_wait` for the one piece
    /// of debug behaviour that is *not* declared here.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub debug: Option<u16>,
}

#[derive(Debug, Serialize, Clone)]
pub struct NodeBuild {
    /// The build context: a folder of the node's repo or, with `source`, of
    /// the source's clone (`.` for its root).
    pub context: String,
    pub dockerfile: String,
    /// The Dockerfile itself, sent in the build tar as `.fghj.Dockerfile`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub dockerfile_inline: Option<String>,
    /// Set when `build.context` is a git URL: the code comes from there, the
    /// definition from the node's own repo. See
    /// `concepts/git-build-sources.md`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source: Option<BuildSource>,
    pub args: BTreeMap<String, String>,
    /// `docker build --target`. `None` builds the Dockerfile's final stage.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub target: Option<String>,
    /// Forward the workspace owner's ssh-agent into the build — see
    /// `concepts/build-inputs.md`.
    #[serde(default)]
    pub ssh: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub secrets: Vec<NodeBuildSecret>,
}

/// The clone a git build context is built from.
#[derive(Debug, Serialize, Clone)]
pub struct BuildSource {
    pub url: String,
    /// Branch, tag or commit. `None` is the remote's default branch.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reference: Option<String>,
    /// Workspace-relative: `.fghj/sources/<name>@<ref>`.
    pub path: String,
    pub downloaded: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub head: Option<String>,
    #[serde(default)]
    pub dirty: bool,
}

/// The checkout a built node's image comes from: the clone for a git build
/// context, otherwise the node's own repo. What drift, the container labels
/// and the Drawer's "built from" all describe.
pub struct Checkout<'a> {
    pub branch: Option<&'a str>,
    pub head: Option<&'a str>,
    pub dirty: bool,
}

impl Node {
    /// `None` for a node running a published `image:`.
    pub fn build_checkout(&self) -> Option<Checkout<'_>> {
        let build = self.build.as_ref()?;
        Some(match &build.source {
            Some(source) => Checkout {
                branch: source.reference.as_deref(),
                head: source.head.as_deref(),
                dirty: source.dirty,
            },
            None => Checkout {
                branch: self.branch.as_deref(),
                head: self.head.as_deref(),
                dirty: self.dirty,
            },
        })
    }
}

/// One `--mount=type=secret` source, resolved against the repo checkout root
/// at build time — see `#BuildSecret` in `schema/component.cue`.
#[derive(Debug, Serialize, Clone)]
pub struct NodeBuildSecret {
    pub id: String,
    pub file: String,
}

/// `from` is always the dependent, `to` always the dependency.
#[derive(Debug, Serialize, Clone)]
pub struct Edge {
    pub from: String,
    pub to: String,
    /// Always "depends-on": a `depends_on` entry. A `depends_on` on another
    /// repo's flow becomes one of these per member of that flow. Hostname
    /// templates are not edges — see `concepts/dependency-kinds.md`.
    pub kind: String,
    /// `depends_on`'s `required`: `true` is "needed to start" (orders,
    /// waits, blocks, and brings `to` into any run `from` is in); `false` is
    /// "needed at runtime", which does none of that. See
    /// [`Edge::needed_to_start`].
    #[serde(default)]
    pub required: bool,
    /// `depends_on`'s `condition`, for a "depends-on" edge.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub condition: Option<String>,
    /// For an edge produced by a `depends_on` on another repo's flow: that
    /// flow's id (`billing/db`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub via_flow: Option<String>,
    pub flows: Vec<String>,
}

impl Edge {
    /// Whether `from` can't start without `to`: the one kind of edge that
    /// orders a start, waits, forms a cycle, and propagates failure, restart
    /// and stop. See `concepts/dependency-kinds.md`.
    pub fn needed_to_start(&self) -> bool {
        self.kind == "depends-on" && self.required
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct Graph {
    pub workspace_name: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Every flow's id (`{repo}/{flow}`), including one that starts
    /// nothing — it still exists, and naming it is not a typo.
    #[serde(default)]
    pub flows: Vec<String>,
    pub warnings: Vec<Warning>,
}

/// `seeds` plus everything they can't start without: the closure over
/// edges that are [`Edge::needed_to_start`].
pub fn required_closure(edges: &[Edge], seeds: BTreeSet<String>) -> BTreeSet<String> {
    let mut set = seeds;
    let mut queue: Vec<String> = set.iter().cloned().collect();
    while let Some(id) = queue.pop() {
        for edge in edges {
            if edge.needed_to_start() && edge.from == id && set.insert(edge.to.clone()) {
                queue.push(edge.to.clone());
            }
        }
    }
    set
}

impl Graph {
    /// The node ids a run of `flow` starts — every node when `None`, which
    /// is what production does. An unknown flow is an error rather than an
    /// empty run, so a typo can't start nothing and report success.
    pub fn start_ids(&self, flow: Option<&str>) -> anyhow::Result<Vec<String>> {
        let Some(flow) = flow else {
            return Ok(self.nodes.iter().map(|n| n.id.clone()).collect());
        };
        if !self.flows.iter().any(|f| f == flow) {
            anyhow::bail!(
                "no flow named '{flow}' — flows are written `{{repo}}/{{flow}}`; this \
                 workspace has: {}",
                if self.flows.is_empty() {
                    "none".to_string()
                } else {
                    self.flows.join(", ")
                }
            );
        }
        Ok(self
            .nodes
            .iter()
            .filter(|n| n.flows.iter().any(|f| f == flow))
            .map(|n| n.id.clone())
            .collect())
    }

    /// What a run of `ids` will get wrong, said before it starts: a node
    /// that needs something at runtime (`required: false`) which this run
    /// doesn't start. Not blocking — starting a flow without a service you
    /// know you won't call is the point of flows. A dependency on another
    /// repo's flow is named once, as the flow, not once per member.
    pub fn start_advisories(&self, ids: &[String]) -> Vec<String> {
        let set: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
        let missing: BTreeSet<(&str, &str)> = self
            .edges
            .iter()
            .filter(|e| e.kind == "depends-on" && !e.required)
            .filter(|e| set.contains(e.from.as_str()) && !set.contains(e.to.as_str()))
            .map(|e| (e.from.as_str(), e.via_flow.as_deref().unwrap_or(&e.to)))
            .collect();
        missing
            .into_iter()
            .map(|(from, to)| {
                format!("'{from}' needs '{to}' at runtime, but this run doesn't start it")
            })
            .collect()
    }

    /// The warnings that must stop a start, in declaration order.
    ///
    /// Resolution never fails: a workspace that can't be fully understood
    /// still resolves into whatever *was* understood, because the UI has to
    /// be able to show you a broken workspace in order for you to fix it.
    /// That is right for a *view* and wrong for an *actuation* — before
    /// this, `resolve_universe` would happily hand a graph containing "'a'
    /// depends on a service name that does not exist" straight to
    /// `RunRegistry::start`, which would then bring up a subset of the
    /// environment that nobody asked for and report success.
    ///
    /// So the severity split lives on the warning and the refusal lives at
    /// the start path: see [`super::warning::Severity`].
    pub fn blocking_warnings(&self) -> Vec<&Warning> {
        self.warnings.iter().filter(|w| w.is_blocking()).collect()
    }

    /// `Err` with every blocking warning listed when this graph must not be
    /// started. One error naming all of them, not the first — they are
    /// usually independent config mistakes, and fixing them one round-trip
    /// at a time is miserable.
    pub fn refuse_if_blocked(&self) -> anyhow::Result<()> {
        let blocking = self.blocking_warnings();
        if blocking.is_empty() {
            return Ok(());
        }
        anyhow::bail!(
            "refusing to start: the workspace has {} unresolved configuration \
             {}:\n{}",
            blocking.len(),
            if blocking.len() == 1 {
                "problem"
            } else {
                "problems"
            },
            blocking
                .iter()
                .map(|w| format!("  - {}", w.message))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::Severity;

    fn graph(warnings: Vec<Warning>) -> Graph {
        Graph {
            workspace_name: "ws".into(),
            nodes: Vec::new(),
            edges: Vec::new(),
            flows: Vec::new(),
            warnings,
        }
    }

    /// Advisories are the common case — a workspace with a cycle, or a repo
    /// that declares no services, must still start.
    #[test]
    fn advisories_alone_do_not_block_a_start() {
        let g = graph(vec![
            Warning::advisory("declares no services"),
            Warning::advisory("dependency cycle: a -> b -> a"),
        ]);
        assert!(g.blocking_warnings().is_empty());
        assert!(g.refuse_if_blocked().is_ok());
    }

    #[test]
    fn a_blocking_warning_refuses_the_start() {
        let g = graph(vec![Warning::blocking("no such service 'api'")]);
        let err = g.refuse_if_blocked().unwrap_err().to_string();
        assert!(err.contains("no such service 'api'"), "{err}");
        assert!(err.contains("1 unresolved configuration problem"), "{err}");
    }

    /// All of them, not just the first: they are usually independent
    /// mistakes, and reporting one per attempt turns fixing a workspace
    /// into a guessing game.
    #[test]
    fn every_blocking_warning_is_named_and_advisories_are_not() {
        let g = graph(vec![
            Warning::blocking("no such service 'api'"),
            Warning::advisory("declares no services"),
            Warning::blocking("'web' depends on itself"),
        ]);
        assert_eq!(g.blocking_warnings().len(), 2);
        let err = g.refuse_if_blocked().unwrap_err().to_string();
        assert!(err.contains("no such service 'api'"), "{err}");
        assert!(err.contains("'web' depends on itself"), "{err}");
        assert!(!err.contains("declares no services"), "{err}");
        assert!(err.contains("2 unresolved configuration problems"), "{err}");
    }

    #[test]
    fn a_clean_graph_starts() {
        assert!(graph(Vec::new()).refuse_if_blocked().is_ok());
        assert_eq!(Warning::blocking("x").severity, Severity::Blocking);
    }
}
