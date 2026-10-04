//! The resolved output model the UI and the run layer consume.

use std::collections::BTreeMap;

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
    /// `concepts/branch-ownership-model.md`. Always `false` for stub
    /// (`downloaded: false`), backing, and flow nodes, which have no checkout.
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
    pub context: String,
    pub dockerfile: String,
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

/// One `--mount=type=secret` source, resolved against the repo checkout root
/// at build time — see `#BuildSecret` in `schema/component.cue`.
#[derive(Debug, Serialize, Clone)]
pub struct NodeBuildSecret {
    pub id: String,
    pub file: String,
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
    pub warnings: Vec<Warning>,
}

impl Graph {
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
