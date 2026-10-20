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
    /// "service" | "backing" | "task". A task's exit 0 is success, not drift.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    /// Filled in by a final pass of `resolve_universe`, which knows the workspace.
    pub domain: String,
    pub downloaded: bool,
    /// Inherited from the owning service for backing dependencies and tasks.
    /// Unreadable git reports `true` with `branch`/`head` `None`, which the UI
    /// shows as "unreadable".
    pub dirty: bool,
    /// Hashed into `spec_hash` only for built nodes: a commit in a repo that
    /// declares `postgres:16` says nothing about that container.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub head: Option<String>,
    pub flows: Vec<String>,
    /// What `${FGHJ_SERVICE_FQDN:alias/name}` looks up; on the node because
    /// `env_file` is only expanded when the node starts.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub includes: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<NodeBuild>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub ports: BTreeMap<String, PortConfig>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub environment: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub command: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub volumes: Vec<VolumeMount>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub additional_hosts: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub wildcard_hosts: Vec<String>,
    /// Resolved at start time: a relative path needs the owning checkout root.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub env_file: Vec<String>,
    #[serde(default = "default_restart")]
    pub restart: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub stop_signal: Option<String>,
    /// Stamped onto the container at create time, so it outlives the `Node`.
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
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub run_policy: Option<String>,
    /// The store this task migrates: everything that waits on the store waits
    /// on this too. See `concepts/seeds-and-migrations.md`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub migrates: Option<String>,
    /// The stores whose data this task seeds; it runs again only on new data.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub seeds: Vec<String>,
    /// Published alongside `ports`; nothing is injected into the environment. See
    /// `concepts/debugging-in-containers.md`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub debug: Option<u16>,
}

#[derive(Debug, Serialize, Clone)]
pub struct NodeBuild {
    pub context: String,
    pub dockerfile: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub dockerfile_inline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source: Option<BuildSource>,
    pub args: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub target: Option<String>,
    #[serde(default)]
    pub ssh: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub secrets: Vec<NodeBuildSecret>,
}

#[derive(Debug, Serialize, Clone)]
pub struct BuildSource {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reference: Option<String>,
    pub path: String,
    pub downloaded: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub head: Option<String>,
    #[serde(default)]
    pub dirty: bool,
}

/// What drift, the container labels and the Drawer's "built from" describe.
pub struct Checkout<'a> {
    pub branch: Option<&'a str>,
    pub head: Option<&'a str>,
    pub dirty: bool,
}

impl Node {
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

#[derive(Debug, Serialize, Clone)]
pub struct NodeBuildSecret {
    pub id: String,
    pub file: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub via_flow: Option<String>,
    pub flows: Vec<String>,
    /// Not written in any file: waiting on a store means waiting on its migration.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub implied: bool,
}

impl Edge {
    /// The one kind of edge that orders a start, waits, forms a cycle, and
    /// propagates failure, restart and stop. See `concepts/dependency-kinds.md`.
    pub fn needed_to_start(&self) -> bool {
        self.kind == "depends-on" && self.required
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct Graph {
    pub workspace_name: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    #[serde(default)]
    pub flows: Vec<String>,
    pub warnings: Vec<Warning>,
}

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

/// Adds the migration of every store in `set`, and what that migration needs:
/// a store isn't ready until it's migrated, whichever way it was started.
pub fn add_migrations<'a>(
    nodes: impl IntoIterator<Item = &'a Node> + Clone,
    edges: &[Edge],
    set: &mut BTreeSet<String>,
) {
    loop {
        let missing: BTreeSet<String> = nodes
            .clone()
            .into_iter()
            .filter(|n| n.migrates.as_ref().is_some_and(|s| set.contains(s)))
            .filter(|n| !set.contains(&n.id))
            .map(|n| n.id.clone())
            .collect();
        if missing.is_empty() {
            return;
        }
        set.extend(required_closure(edges, missing));
    }
}

impl Graph {
    /// `ids` plus the migrations of any store among them. See [`add_migrations`].
    pub fn with_migrations(&self, ids: Vec<String>) -> Vec<String> {
        let mut set: BTreeSet<String> = ids.iter().cloned().collect();
        add_migrations(&self.nodes, &self.edges, &mut set);
        let mut ids = ids;
        ids.extend(
            set.into_iter()
                .filter(|id| !ids.contains(id))
                .collect::<Vec<_>>(),
        );
        ids
    }

    /// An unknown flow is an error, so a typo can't start nothing and succeed.
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

    /// Not blocking: starting a flow without a service you won't call is the point
    /// of flows. Another repo's flow is named once, not once per member.
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

    /// Resolution never fails, so the UI can show a broken workspace; starting one
    /// must. See [`super::warning::Severity`].
    pub fn blocking_warnings(&self) -> Vec<&Warning> {
        self.warnings.iter().filter(|w| w.is_blocking()).collect()
    }

    /// All of them in one error: they're usually independent mistakes.
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
