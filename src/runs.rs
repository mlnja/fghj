use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::docker;
use crate::resolver::{Edge, Graph, Node, VolumeMount};
use crate::store::WorkspaceDb;

pub const DEFAULT_RUN_ID: &str = "default";

fn sanitize_label(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::new();
    for c in lower.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct RunSpec {
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub overrides: BTreeMap<String, String>,
    /// Scopes the run to only the nodes reachable from this flow (see
    /// `Node::flows`) instead of the whole graph — e.g. starting just the
    /// checkout flow's services instead of every service fghj knows about.
    #[serde(default)]
    pub flow: Option<String>,
}

/// Derives a node's canonical `*.fghj.internal` domain for a given run — the
/// single definition `start_node` uses when actually launching a container,
/// also called from `resolver::resolve_universe` (always with
/// `DEFAULT_RUN_ID`) so `Node.domain` can carry a node's default-run address
/// before any container for it has ever been started. Two nodes can never
/// collide on the result: `node_id` is already the unique, leaf-first id
/// (see `resolver::visit_local_service`/`visit_dependency`), and `run_id` is
/// folded in for every run except the default one (see `start_node`'s own
/// comment for why).
pub fn derive_domain(
    node_id: &str,
    domain_scope: &str,
    workspace_name: &str,
    run_id: &str,
) -> String {
    let workspace = sanitize_label(workspace_name);
    if domain_scope == "stable" || run_id == DEFAULT_RUN_ID {
        format!("{node_id}.{workspace}.fghj.internal")
    } else {
        format!("{node_id}.{run_id}.{workspace}.fghj.internal")
    }
}

/// Expands `${FGHJ_SERVICE_FQDN}` (this node's own derived domain) and
/// `${FGHJ_SERVICE_FQDN:path}` (a sibling's domain — see `sibling_domain`
/// for what `path` can look like) in a single `environment`/`env_file`
/// value, so a CUE author can reference a `*.fghj.internal` address without
/// hand-computing `derive_domain`'s formula into a literal string (the
/// convention every hardcoded `*_HOST`/`*_URL` value in
/// `aikido-core`/`aikifactory`'s `.fghj.yaml` followed before this
/// existed). A manual scan rather than the `regex` crate (not otherwise a
/// dependency) — the grammar is just those two forms, simple enough that a
/// scanner is less code than pulling in a new crate. An unresolvable
/// `:path` (no such sibling) or a token missing its closing `}` is left
/// untouched in the output rather than erroring — a typo here shouldn't
/// fail an entire run when the literal fallback is at least diagnosable in
/// logs, the same tolerance `parse_env_file` extends to a malformed line.
fn expand_service_fqdn_templates(
    value: &str,
    node: &Node,
    own_domain: &str,
    graph: &Graph,
    run_id: &str,
) -> String {
    const TOKEN: &str = "${FGHJ_SERVICE_FQDN";
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find(TOKEN) {
        out.push_str(&rest[..start]);
        let Some(end_rel) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let end = start + end_rel;
        let inner = &rest[start + TOKEN.len()..end]; // "" or ":name"
        let resolved = match inner.strip_prefix(':') {
            None => Some(own_domain.to_string()),
            Some(name) => sibling_domain(node, name, graph, run_id),
        };
        match resolved {
            Some(domain) => out.push_str(&domain),
            None => out.push_str(&rest[start..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Finds the domain `${FGHJ_SERVICE_FQDN:path}` means from `node`'s own
/// `environment`, in two ways (first match wins):
///
/// - A backing dependency matching `path` that shares an "owns" owner with
///   `node` — the owner is whoever's "owns" edge points at `node` (a
///   backing dependency looking for a sibling backing dependency), or
///   `node.id` itself if nothing owns it (a service looking up one of its
///   own directly-declared backing dependencies).
/// - A service matching `path` that `node` directly depends on via a
///   `kind: service` dependency (same-repo or cross-repo — both produce a
///   "depends-on" edge from `node.id`, see `resolver::visit_dependency`).
///   This is the only way to reach a sibling *service*: unlike backing
///   dependencies, services aren't owned, so there's no shared-owner case
///   to fall back on — only what `node` itself declares a dependency on.
///
/// `path` is one bare name (`mysql`) in the common case — matched against
/// just the candidate's own leaf name — or `::`-separated segments
/// (`aikifactory::aikifactory::minio`) for the rare case where that's
/// ambiguous. A node's `id` is already the leaf-first chain the domain
/// itself is built from (`{name}.{owner-id}`, see `resolver::visit_dependency`
/// /`visit_local_services`) — root-first is just easier to read/write, so
/// `path`'s segments are reversed and dot-joined into that same shape
/// before matching, e.g. `aikifactory::aikifactory::minio` becomes
/// `minio.aikifactory.aikifactory`, an exact prefix of the real id
/// `minio.aikifactory.aikifactory` (before the workspace/`fghj.internal`
/// suffix `derive_domain` appends). Fewer segments than the full id just
/// means "match any id with this as a trailing-toward-the-root prefix" —
/// as many as it takes to stop being ambiguous, no more.
fn sibling_domain(node: &Node, path: &str, graph: &Graph, run_id: &str) -> Option<String> {
    let mut segments: Vec<&str> = path.split("::").collect();
    segments.reverse();
    let id_prefix = segments.join(".");
    let matches = |candidate: &Node| {
        candidate.id == id_prefix || candidate.id.starts_with(&format!("{id_prefix}."))
    };

    let owner_id = graph
        .edges
        .iter()
        .find(|e| e.kind == "owns" && e.to == node.id)
        .map(|e| e.from.as_str())
        .unwrap_or(node.id.as_str());
    let sibling = graph
        .edges
        .iter()
        .filter(|e| e.kind == "owns" && e.from == owner_id)
        .find_map(|e| graph.nodes.iter().find(|n| n.id == e.to && matches(n)))
        .or_else(|| {
            graph
                .edges
                .iter()
                .filter(|e| e.kind == "depends-on" && e.from == node.id)
                .find_map(|e| graph.nodes.iter().find(|n| n.id == e.to && matches(n)))
        })?;
    Some(derive_domain(
        &sibling.id,
        &sibling.domain_scope,
        &graph.workspace_name,
        run_id,
    ))
}

/// Derives the real Docker volume name for a `VolumeMount::Named` entry —
/// reuses `derive_domain`'s exact run/stable folding logic (a named
/// volume's `scope` is the same knob as `domain_scope`), keyed by the
/// volume's own declared `name` instead of a node id. Two nodes anywhere in
/// the graph that declare the same `name` + `scope` therefore land on the
/// same derived value here and transparently share one Docker volume.
fn derive_volume_name(name: &str, scope: &str, workspace_name: &str, run_id: &str) -> String {
    format!(
        "fghj-vol-{}",
        sanitize_label(&derive_domain(name, scope, workspace_name, run_id))
    )
}

/// Orders `node_ids` so that every node's dependencies — an edge's `to` (see
/// `resolver::Edge`'s own doc comment: `to` is always the dependency, `from`
/// always the dependent, for every edge kind) — are started before it. Used
/// by both `start` and `ensure_running` so containers come up in dependency
/// order instead of whatever order `graph.nodes` happens to iterate in.
/// Edges pointing outside `node_ids` (e.g. a flow-filtered run that excludes
/// a node's dependency) are ignored — nothing to order against.
///
/// A cycle can't make progress by definition; rather than fail the whole
/// run over a cyclic `.fghj.yaml` (resolution here is independent of `fghj
/// validate` — see the module doc on that split), whatever's left over is
/// appended in stable sorted order so a run still starts *something*.
pub fn topological_start_order(node_ids: &[String], edges: &[Edge]) -> Vec<String> {
    let ids: HashSet<&str> = node_ids.iter().map(|s| s.as_str()).collect();
    let mut deps: HashMap<&str, HashSet<&str>> = HashMap::new();
    for id in node_ids {
        deps.entry(id.as_str()).or_default();
    }
    for edge in edges {
        if ids.contains(edge.from.as_str()) && ids.contains(edge.to.as_str()) {
            deps.entry(edge.from.as_str())
                .or_default()
                .insert(edge.to.as_str());
        }
    }

    let mut ordered: Vec<String> = Vec::new();
    let mut placed: HashSet<&str> = HashSet::new();
    // Sorted, not hashmap-iteration-order, so ties (and the cycle fallback
    // below) are stable across calls.
    let mut remaining: Vec<&str> = node_ids.iter().map(|s| s.as_str()).collect();
    remaining.sort_unstable();

    while !remaining.is_empty() {
        let mut next_remaining = Vec::new();
        let mut progressed = false;
        for id in &remaining {
            if deps[id].iter().all(|d| placed.contains(d)) {
                ordered.push(id.to_string());
                placed.insert(id);
                progressed = true;
            } else {
                next_remaining.push(*id);
            }
        }
        remaining = next_remaining;
        if !progressed {
            ordered.extend(remaining.into_iter().map(str::to_string));
            break;
        }
    }
    ordered
}

/// Polls a container's declared healthcheck (if any) until it reports
/// "healthy", for up to two minutes — long enough for a real database's own
/// startup healthcheck, short enough that a genuinely broken one doesn't
/// hang a run forever. Returns as soon as there's nothing more to wait for:
/// no declared healthcheck, a terminal "unhealthy" report (best-effort —
/// fghj proceeds rather than blocking the run indefinitely), or the
/// container having vanished. Callers only call this at all when
/// `node.healthcheck.is_some()`, but it's written to be a safe no-op
/// otherwise too.
async fn wait_for_healthy(docker: &bollard::Docker, container_name: &str) {
    for _ in 0..60 {
        match docker::inspect_health(docker, container_name).await {
            Ok(Some(status)) if status == "healthy" || status == "unhealthy" => return,
            Ok(None) => return,
            _ => {}
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Parses a `.env`-style file's contents into "KEY=value" pairs — blank
/// lines and `#`-comments are skipped, and matching surrounding quotes on
/// the value are stripped (the common `.env` convention). No multi-line
/// values or `export` prefixes: real `.env` files in the wild are simple
/// enough that this covers the practical cases, same scope Compose's own
/// `env_file` support covers.
fn parse_env_file(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            let mut value = value.trim();
            if value.len() >= 2
                && ((value.starts_with('"') && value.ends_with('"'))
                    || (value.starts_with('\'') && value.ends_with('\'')))
            {
                value = &value[1..value.len() - 1];
            }
            Some(format!("{key}={value}"))
        })
        .collect()
}

/// A `*.fghj.internal` name this container answers to, and the `127.0.0.1`
/// port Docker actually published its backing container-side port on — the
/// SNI -> backend lookup `proxy::serve_https` dispatches real per-service
/// HTTPS routing through (see `WorkspaceRegistry::resolve_route`).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PortRoute {
    pub domain: String,
    pub host_port: u16,
    /// When set, `domain` is a suffix from `Node.wildcard_hosts` rather than
    /// an exact name — `WorkspaceRegistry::resolve_route` matches it against
    /// the suffix itself *and* any subdomain of it, not just an exact
    /// string. `#[serde(default)]` so rows persisted before this field
    /// existed just deserialize as `false` (an ordinary exact route).
    #[serde(default)]
    pub wildcard: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct ContainerInfo {
    pub node_id: String,
    pub container_name: String,
    pub status: String,
    pub published_port: Option<u16>,
    pub domain: String,
    pub routes: Vec<PortRoute>,
    /// The subset of `Node.additional_hosts` that actually got a route (i.e.
    /// the node has a `primary` port) — kept separate from `routes` (which
    /// also carries the node's own derived-domain and named-port routes)
    /// because `daemon::WorkspaceRegistry::active_additional_hosts` needs
    /// exactly this list, and only this list, to sync `/etc/hosts`: a
    /// `*.fghj.internal` route is already served by fghjd's own DNS, and
    /// would be actively wrong to also pin as a static `/etc/hosts` entry.
    #[serde(default)]
    pub additional_hosts: Vec<String>,
    /// The host-published port for every one of this node's declared ports,
    /// not just the routed (`primary`/`name`d) ones — lets the UI offer a
    /// direct `127.0.0.1:<port>` connection string for a plain TCP backing
    /// dependency (postgres, mysql) that has no HTTP surface to route at
    /// all, alongside the `*.fghj.internal` links `routes` already covers.
    /// `None` for a port Docker hasn't actually published (container not
    /// running, or the port entry has no live binding yet).
    #[serde(default)]
    pub ports: BTreeMap<String, Option<u16>>,
}

#[derive(Debug, Serialize, Clone)]
pub struct RunState {
    pub run_id: String,
    pub overrides: BTreeMap<String, String>,
    pub network: String,
    pub containers: Vec<ContainerInfo>,
}

pub struct RunRegistry {
    workspace: std::path::PathBuf,
    db: Arc<WorkspaceDb>,
    docker: Arc<bollard::Docker>,
    runs: Mutex<BTreeMap<String, RunState>>,
}

impl RunRegistry {
    /// Loads any runs persisted from a previous `fghjd` lifetime and
    /// reconciles each against real docker state: a run whose containers are
    /// all still alive is restored with freshly-inspected statuses, and a run
    /// missing any container (removed out-of-band, or lost across a reboot
    /// with no restart policy) is dropped rather than presented as running.
    pub async fn new(
        workspace: std::path::PathBuf,
        db: Arc<WorkspaceDb>,
        docker: Arc<bollard::Docker>,
    ) -> Result<Self> {
        let persisted = db.clone().load_runs().await?;
        let mut reconciled = BTreeMap::new();
        for (run_id, mut state) in persisted {
            let mut alive = true;
            for c in &mut state.containers {
                match docker::inspect_status(&docker, &c.container_name, "").await {
                    Ok(Some(status)) => c.status = status.status,
                    _ => {
                        alive = false;
                        break;
                    }
                }
            }
            if alive {
                reconciled.insert(run_id, state);
            } else {
                let _ = db.clone().delete_run(run_id).await;
            }
        }
        Ok(Self {
            workspace,
            db,
            docker,
            runs: Mutex::new(reconciled),
        })
    }

    pub fn list(&self) -> Vec<RunState> {
        self.runs.lock().unwrap().values().cloned().collect()
    }

    /// Re-inspects every live run's containers against real docker state and
    /// updates their recorded status in place — including flagging any
    /// container that's vanished (e.g. `docker rm`'d by hand, outside fghj)
    /// as `"removed"` — so the next `/runs` poll reflects reality instead of
    /// a snapshot frozen at whenever the run last started or was persisted.
    /// Purely observational: it never touches docker itself.
    ///
    /// Snapshots the container names while holding the lock, inspects them
    /// all without holding it (inspection is an async docker call), then
    /// re-locks to write results back — the lock is never held across an
    /// `.await`.
    pub async fn refresh(&self) {
        let snapshot: Vec<(String, Vec<String>)> = {
            let runs = self.runs.lock().unwrap();
            runs.iter()
                .map(|(run_id, state)| {
                    (
                        run_id.clone(),
                        state
                            .containers
                            .iter()
                            .map(|c| c.container_name.clone())
                            .collect(),
                    )
                })
                .collect()
        };

        let mut results: Vec<(String, Vec<String>)> = Vec::new();
        for (run_id, container_names) in snapshot {
            let mut statuses = Vec::new();
            for name in container_names {
                let status = match docker::inspect_status(&self.docker, &name, "").await {
                    Ok(Some(s)) => s.status,
                    _ => "removed".to_string(),
                };
                statuses.push(status);
            }
            results.push((run_id, statuses));
        }

        let mut changed_states: Vec<RunState> = Vec::new();
        {
            let mut runs = self.runs.lock().unwrap();
            for (run_id, statuses) in results {
                if let Some(state) = runs.get_mut(&run_id) {
                    let mut changed = false;
                    for (c, status) in state.containers.iter_mut().zip(statuses) {
                        if status != c.status {
                            c.status = status;
                            changed = true;
                        }
                    }
                    if changed {
                        changed_states.push(state.clone());
                    }
                }
            }
        }
        for state in changed_states {
            let _ = self.db.clone().save_run(state).await;
        }
    }

    pub fn get(&self, run_id: &str) -> Option<RunState> {
        self.runs.lock().unwrap().get(run_id).cloned()
    }

    pub async fn stop(&self, run_id: &str) -> Result<()> {
        let state = {
            let mut runs = self.runs.lock().unwrap();
            let Some(state) = runs.remove(run_id) else {
                bail!("no such run: {run_id}");
            };
            state
        };
        for c in &state.containers {
            docker::stop_and_remove(&self.docker, &c.container_name).await;
        }
        docker::remove_network(&self.docker, &state.network).await;
        // The default run's `scope: "run"` volumes get the exact same
        // derived name on every start (`derive_volume_name` only folds the
        // run id in for a *named* run) — deleting them here would silently
        // wipe data a plain stop+restart expects to still be there. Only a
        // named/preview run's volumes are safe to clean up.
        if run_id != DEFAULT_RUN_ID {
            docker::remove_run_scoped_volumes(&self.docker, run_id).await;
        }
        self.db.clone().delete_run(run_id.to_string()).await?;
        Ok(())
    }

    pub async fn start(&self, graph: &Graph, spec: RunSpec) -> Result<RunState> {
        let run_id = spec
            .run_id
            .as_deref()
            .map(sanitize_label)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_RUN_ID.to_string());

        // starting an already-running run replaces it cleanly
        let already_running = self.runs.lock().unwrap().contains_key(&run_id);
        if already_running {
            self.stop(&run_id).await?;
        }

        let network = format!("fghj-{}-{}", sanitize_label(&graph.workspace_name), run_id);
        docker::ensure_network(&self.docker, &network, &network).await?;

        let owner = self.db.clone().load_owner().await.ok().flatten();

        let node_map: HashMap<&str, &Node> =
            graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let target_ids: Vec<String> = graph
            .nodes
            .iter()
            .filter(|n| n.kind != "flow")
            .filter(|n| {
                spec.flow
                    .as_deref()
                    .is_none_or(|flow| n.flows.iter().any(|f| f == flow))
            })
            .map(|n| n.id.clone())
            .collect();
        let ordered_ids = topological_start_order(&target_ids, &graph.edges);

        let mut containers = Vec::new();
        for node_id in &ordered_ids {
            let node = node_map[node_id.as_str()];
            match self
                .start_node(
                    graph,
                    node,
                    &run_id,
                    &network,
                    &spec.overrides,
                    owner.as_ref(),
                )
                .await
            {
                Ok(info) => {
                    if node.healthcheck.is_some() {
                        wait_for_healthy(&self.docker, &info.container_name).await;
                    }
                    containers.push(info);
                }
                Err(e) => {
                    for c in &containers {
                        docker::stop_and_remove(&self.docker, &c.container_name).await;
                    }
                    docker::remove_network(&self.docker, &network).await;
                    return Err(e);
                }
            }
        }

        let state = RunState {
            run_id: run_id.clone(),
            overrides: spec.overrides,
            network,
            containers,
        };
        self.db.clone().save_run(state.clone()).await?;
        self.runs.lock().unwrap().insert(run_id, state.clone());
        Ok(state)
    }

    /// Tops up the single default environment so every node reachable from
    /// `flow` (or every node in the graph, if `flow` is `None`) is running —
    /// unlike `start`, this never touches a container that's already alive.
    /// fghj models one shared set of running containers per workspace, not a
    /// separate environment per flow, so picking a flow should never restart
    /// (or duplicate) whatever's already up.
    ///
    /// Liveness is checked directly against docker on every call rather than
    /// trusting the persisted `RunState`, since a container can be
    /// stopped/removed out-of-band between calls (see `refresh`).
    pub async fn ensure_running(&self, graph: &Graph, flow: Option<&str>) -> Result<RunState> {
        let run_id = DEFAULT_RUN_ID.to_string();
        let network = format!("fghj-{}-{}", sanitize_label(&graph.workspace_name), run_id);
        docker::ensure_network(&self.docker, &network, &network).await?;

        let mut state = self
            .runs
            .lock()
            .unwrap()
            .get(&run_id)
            .cloned()
            .unwrap_or_else(|| RunState {
                run_id: run_id.clone(),
                overrides: BTreeMap::new(),
                network: network.clone(),
                containers: Vec::new(),
            });

        let owner = self.db.clone().load_owner().await.ok().flatten();

        let node_map: HashMap<&str, &Node> =
            graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let target_ids: Vec<String> = graph
            .nodes
            .iter()
            .filter(|n| n.kind != "flow")
            .filter(|n| flow.is_none_or(|flow| n.flows.iter().any(|f| f == flow)))
            .map(|n| n.id.clone())
            .collect();
        let ordered_ids = topological_start_order(&target_ids, &graph.edges);

        for node_id in &ordered_ids {
            let node = node_map[node_id.as_str()];
            let container_name = format!(
                "fghj-{}-{}-{}",
                sanitize_label(&graph.workspace_name),
                run_id,
                sanitize_label(&node.id)
            );
            let alive = matches!(
                docker::inspect_status(&self.docker, &container_name, "").await,
                Ok(Some(s)) if s.status == "running"
            );
            if alive {
                continue;
            }
            // A stopped-but-not-removed container from a previous run would
            // otherwise collide with create_container's fixed name.
            docker::stop_and_remove(&self.docker, &container_name).await;

            let info = self
                .start_node(
                    graph,
                    node,
                    &run_id,
                    &network,
                    &BTreeMap::new(),
                    owner.as_ref(),
                )
                .await?;
            if node.healthcheck.is_some() {
                wait_for_healthy(&self.docker, &info.container_name).await;
            }
            state.containers.retain(|c| c.node_id != info.node_id);
            state.containers.push(info);
            // Saved after every node, not just at the end, so a later
            // failure in this same call doesn't lose track of containers
            // that did start successfully.
            self.db.clone().save_run(state.clone()).await?;
            self.runs
                .lock()
                .unwrap()
                .insert(run_id.clone(), state.clone());
        }

        Ok(state)
    }

    async fn start_node(
        &self,
        graph: &Graph,
        node: &Node,
        run_id: &str,
        network: &str,
        overrides: &BTreeMap<String, String>,
        owner: Option<&crate::store::WorkspaceOwner>,
    ) -> Result<ContainerInfo> {
        let workspace = sanitize_label(&graph.workspace_name);
        let container_name = format!("fghj-{workspace}-{run_id}-{}", sanitize_label(&node.id));
        // Every node's domain is derived the same way, unconditionally —
        // there's no CUE-declared override for any node kind (services
        // included) that could bypass this, so two nodes can never collide
        // on a name the way a hand-written one could. Built from `node.id`
        // rather than `node.label`: `id` is a unique, leaf-first dotted
        // chain (`dep-name.owning-service-id` for backing deps — see
        // `resolver::visit_dependency` — or `service-name.repo-local-path`
        // for services — see `resolver::visit_local_service`), while `label`
        // is only the bare declared name and can collide, e.g. when two
        // peer repos each declare a same-named service, or two different
        // services each own their own same-named backing dependency.
        // `run_id` is folded in just like it is for
        // `container_name`/the network name above, *except* for the default
        // run: fghj models one shared, singular default environment per
        // workspace (see `ensure_running`), so it needs no disambiguating
        // segment — only a named/review run does, since more than one of
        // those can be alive at once. `node.domain_scope == "stable"` is the
        // other opt-out (CUE `#Service.domain_scope` /
        // `#BackingDependency.domain_scope`): a deliberate, explicit choice
        // by the CUE author to give a node one fixed identity shared across
        // every run, not just the default one.
        //
        // This is also the sole Docker network alias registered below, so
        // it resolves identically whether asked from inside this run's
        // docker network (Docker's own embedded DNS) or from the host
        // (fghjd's DNS server, which answers any name in the zone).
        let domain = derive_domain(&node.id, &node.domain_scope, &graph.workspace_name, run_id);

        // Where a node's relative bind-mount `host` / `env_file` paths
        // resolve against — the repo's checkout root, not `build.context`
        // (Compose resolves both relative to the compose file's directory;
        // this is the fghj equivalent). For a service, its own checkout
        // root; for a backing dependency, which has no checkout of its own,
        // the *owning* service's checkout root (set below, via the graph's
        // "owns" edge).
        let mut volume_base: Option<PathBuf> = None;

        let image = match node.kind.as_str() {
            "backing" => {
                // A backing dependency has no checkout of its own to resolve
                // a relative `env_file` (or bind-mount `host`) path against —
                // same rule Compose uses, resolving `env_file` against the
                // compose file's own directory regardless of `build` vs
                // `image`. Its equivalent of "the compose file's directory"
                // is the *owning* service's checkout root: the service whose
                // .fghj.yaml declares this dependency inline, found via the
                // graph's "owns" edge (`resolver::visit_dependency` always
                // pushes owner -> backing).
                let owner_local_path = graph
                    .edges
                    .iter()
                    .find(|e| e.kind == "owns" && e.to == node.id)
                    .and_then(|e| graph.nodes.iter().find(|n| n.id == e.from))
                    .and_then(|n| n.local_path.as_ref());
                if let Some(owner_local_path) = owner_local_path {
                    volume_base = Some(self.workspace.join(owner_local_path));
                }
                match node.image.clone() {
                    Some(img) => img,
                    None => bail!("backing node {} has no image", node.id),
                }
            }
            _ => {
                let build = node.build.clone().unwrap_or(crate::resolver::NodeBuild {
                    context: ".".to_string(),
                    dockerfile: "Dockerfile".to_string(),
                    args: BTreeMap::new(),
                });

                match overrides.get(&node.id) {
                    // Branch override: build from a throwaway checkout of that
                    // branch, leaving the live workspace dir untouched.
                    Some(branch) => {
                        let repo = match node.repo.clone() {
                            Some(r) => r,
                            None => bail!("service node {} has no repo", node.id),
                        };
                        let tag = format!(
                            "fghj/{}:{}",
                            sanitize_label(&node.id),
                            sanitize_label(branch)
                        );
                        let internal_dir = self.workspace.join(".fghj");
                        let mirror_dir = internal_dir.clone();
                        let owner_for_mirror = owner.cloned();
                        let mirror = tokio::task::spawn_blocking(move || {
                            crate::resolver::ensure_mirror(
                                &repo,
                                &mirror_dir,
                                owner_for_mirror.as_ref(),
                            )
                        })
                        .await
                        .context("ensure_mirror task panicked")??;
                        let checkout = internal_dir.join("checkouts").join(format!(
                            "{}-{}",
                            sanitize_label(&node.id),
                            sanitize_label(branch)
                        ));
                        let checkout_root =
                            docker::materialize_checkout(&mirror, branch, &checkout).await?;
                        volume_base = Some(checkout_root.clone());
                        let build_dir = checkout_root.join(&build.context);
                        docker::build_image(
                            &self.docker,
                            &build_dir,
                            &build.dockerfile,
                            &tag,
                            node.platform.as_deref(),
                        )
                        .await?;
                        tag
                    }
                    // Default: build straight from the live workspace checkout,
                    // so local edits are picked up on every run.
                    None => {
                        let local_path = match node.local_path.clone() {
                            Some(p) => p,
                            None => bail!("service node {} has no local_path", node.id),
                        };
                        let branch = node.branch.clone().unwrap_or_else(|| "local".to_string());
                        let tag = format!(
                            "fghj/{}:{}",
                            sanitize_label(&node.id),
                            sanitize_label(&branch)
                        );
                        let repo_root = self.workspace.join(&local_path);
                        volume_base = Some(repo_root.clone());
                        let build_dir = repo_root.join(&build.context);
                        docker::build_image(
                            &self.docker,
                            &build_dir,
                            &build.dockerfile,
                            &tag,
                            node.platform.as_deref(),
                        )
                        .await?;
                        tag
                    }
                }
            }
        };

        // Named ports (`#Port.name`) get their own domain, nested under this
        // node's — `admin.api.default.shop.fghj.internal` — and need to be
        // real Docker aliases too, or they'd resolve from the host (fghjd's
        // DNS answers anything in the zone) but not from sibling containers,
        // breaking the same inside/outside consistency the primary domain
        // relies on.
        let mut aliases = vec![domain.clone()];
        aliases.extend(
            node.ports
                .values()
                .filter_map(|p| p.name.as_ref())
                .map(|name| format!("{name}.{domain}")),
        );

        let port_list: Vec<(String, Option<u16>)> = node
            .ports
            .iter()
            .map(|(port, cfg)| (port.clone(), cfg.host_port))
            .collect();

        // A named volume's Docker-side existence is otherwise implicit (the
        // daemon auto-creates one, unlabeled, the first time a bind
        // references it) — `ensure_volume` here labels it so
        // `docker::remove_run_scoped_volumes` can find it in `stop()`.
        // `Iterator::map` can't `.await`, hence the explicit loop.
        let mut binds: Vec<String> = Vec::with_capacity(node.volumes.len());
        for v in &node.volumes {
            match v {
                VolumeMount::Bind {
                    host,
                    container,
                    read_only,
                } => {
                    let host_path = if Path::new(host).is_absolute() {
                        PathBuf::from(host)
                    } else {
                        volume_base
                            .as_ref()
                            .expect("node with volumes has a resolved checkout root")
                            .join(host)
                    };
                    binds.push(format!(
                        "{}:{container}{}",
                        host_path.display(),
                        if *read_only { ":ro" } else { "" }
                    ));
                }
                VolumeMount::Named {
                    name,
                    scope,
                    container,
                    read_only,
                } => {
                    let volume_name =
                        derive_volume_name(name, scope, &graph.workspace_name, run_id);
                    docker::ensure_volume(
                        &self.docker,
                        &volume_name,
                        &graph.workspace_name,
                        scope,
                        run_id,
                    )
                    .await?;
                    binds.push(format!(
                        "{volume_name}:{container}{}",
                        if *read_only { ":ro" } else { "" }
                    ));
                }
            }
        }

        // `env_file` entries load first, in declared order, then
        // `environment` is applied on top — same precedence as Compose,
        // relying on Docker's own last-value-wins behavior for a flat `-e`
        // list rather than de-duping keys here. Relative paths resolve
        // against `volume_base` — this node's own checkout root for a
        // service, or the owning service's for a backing dependency.
        let mut env: Vec<String> = Vec::new();
        for path in &node.env_file {
            let file_path = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                volume_base
                    .as_ref()
                    .expect("node with env_file has a resolved checkout root")
                    .join(path)
            };
            let contents = std::fs::read_to_string(&file_path)
                .with_context(|| format!("failed to read env_file {}", file_path.display()))?;
            env.extend(parse_env_file(&contents));
        }
        env.extend(node.environment.iter().cloned());
        for entry in &mut env {
            *entry = expand_service_fqdn_templates(entry, node, &domain, graph, run_id);
        }

        docker::run_container(
            &self.docker,
            &docker::RunOpts {
                name: &container_name,
                network,
                aliases: &aliases,
                env: &env,
                ports: &port_list,
                image: &image,
                command: &node.command,
                project: network,
                service_name: &node.id,
                binds: &binds,
                restart_policy: &node.restart,
                user: node.user.as_deref(),
                working_dir: node.working_dir.as_deref(),
                labels: &node.labels,
                cap_add: &node.cap_add,
                cap_drop: &node.cap_drop,
                privileged: node.privileged,
                extra_hosts: &node.extra_hosts,
                healthcheck: node.healthcheck.as_ref(),
                platform: node.platform.as_deref(),
            },
        )
        .await?;

        // Prefer the port explicitly marked `primary` — the one actually
        // meant to be "the" entrypoint — over an arbitrary map-iteration
        // order (a `BTreeMap<String, _>` sorts port numbers as strings, so
        // e.g. "10000" would otherwise sort before "9000").
        let status_port = node
            .ports
            .iter()
            .find(|(_, cfg)| cfg.primary)
            .map(|(port, _)| port.clone())
            .or_else(|| node.ports.keys().next().cloned());
        let inspected = match &status_port {
            Some(p) => docker::inspect_status(&self.docker, &container_name, p).await?,
            None => docker::inspect_status(&self.docker, &container_name, "").await?,
        };
        let (status, published_port) = match inspected {
            Some(s) => (s.status, s.published_port),
            None => ("unknown".to_string(), None),
        };

        // Every declared port's actual host-published binding, not just the
        // routed ones — a plain TCP backing dependency (postgres, mysql)
        // has no `primary`/`name`d port to route at all, but the UI still
        // wants a `127.0.0.1:<port>` connection string for it. Reuses the
        // inspect already done above for `status_port`'s own binding rather
        // than re-querying it; one more inspect per remaining port (there's
        // rarely more than one or two per node).
        let mut port_host_ports: BTreeMap<String, Option<u16>> = BTreeMap::new();
        for port in node.ports.keys() {
            let host_port = if status_port.as_deref() == Some(port.as_str()) {
                published_port
            } else {
                docker::inspect_status(&self.docker, &container_name, port)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|s| s.published_port)
            };
            port_host_ports.insert(port.clone(), host_port);
        }

        // Every port with a domain — the `primary` one, at this node's own
        // domain, and/or any `name`d one, at `{name}.{domain}` (a port can
        // be both) — gets a route to the host port Docker actually
        // published it on. `fghjd` runs on the host, not inside the docker
        // network, so it can't resolve these names the way sibling
        // containers do (via Docker's embedded per-network DNS, which only
        // answers from inside that network) — this is what lets
        // `proxy::serve_https` dispatch an incoming SNI straight to the
        // right container instead.
        let mut routes = Vec::new();
        for (port, cfg) in &node.ports {
            if !cfg.primary && cfg.name.is_none() {
                continue;
            }
            let Some(host_port) = port_host_ports.get(port).copied().flatten() else {
                continue;
            };
            if cfg.primary {
                routes.push(PortRoute {
                    domain: domain.clone(),
                    host_port,
                    wildcard: cfg.wildcard,
                });
            }
            if let Some(name) = &cfg.name {
                routes.push(PortRoute {
                    domain: format!("{name}.{domain}"),
                    host_port,
                    wildcard: cfg.wildcard,
                });
            }
        }

        // `#AdditionalHost` aliases route to the same host port as the
        // node's own primary domain — same "multiple names, one backend
        // port" pattern as a named port, just keyed off a literal
        // author-declared hostname instead of a derived one. Silently
        // dropped (not an error) if there's no primary-port route to attach
        // to — `resolver::check_ports` already warns about exactly this at
        // graph-resolution time.
        let mut additional_hosts_active = Vec::new();
        if let Some(host_port) = routes
            .iter()
            .find(|r| r.domain == domain)
            .map(|r| r.host_port)
        {
            for host in &node.additional_hosts {
                routes.push(PortRoute {
                    domain: host.clone(),
                    host_port,
                    wildcard: false,
                });
                additional_hosts_active.push(host.clone());
            }
            for suffix in &node.wildcard_hosts {
                routes.push(PortRoute {
                    domain: suffix.clone(),
                    host_port,
                    wildcard: true,
                });
            }
        }

        Ok(ContainerInfo {
            node_id: node.id.clone(),
            container_name,
            status,
            published_port,
            domain,
            routes,
            additional_hosts: additional_hosts_active,
            ports: port_host_ports,
        })
    }
}

pub async fn logs_for_tail(
    docker: &bollard::Docker,
    state: &RunState,
    node_id: &str,
    tail: usize,
) -> Result<String> {
    let Some(c) = state.containers.iter().find(|c| c.node_id == node_id) else {
        bail!("no such node in run: {node_id}");
    };
    docker::logs_tail(docker, &c.container_name, tail).await
}

/// Returns the container name backing `node_id` in `state`, for the SSE
/// live-follow endpoint to build a `docker::logs_follow` stream from.
pub fn container_name_for<'a>(state: &'a RunState, node_id: &str) -> Result<&'a str> {
    state
        .containers
        .iter()
        .find(|c| c.node_id == node_id)
        .map(|c| c.container_name.as_str())
        .ok_or_else(|| anyhow::anyhow!("no such node in run: {node_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_label_lowercases_and_collapses_separators() {
        assert_eq!(
            sanitize_label("Feature/JIRA-123 Fix"),
            "feature-jira-123-fix"
        );
        assert_eq!(sanitize_label("already-clean"), "already-clean");
        assert_eq!(sanitize_label("__leading__"), "leading");
    }

    fn edge(from: &str, to: &str, kind: &str) -> Edge {
        Edge {
            from: from.to_string(),
            to: to.to_string(),
            kind: kind.to_string(),
            branch: None,
            flows: Vec::new(),
        }
    }

    #[test]
    fn topological_start_order_places_dependencies_before_dependents() {
        let ids = vec!["app".to_string(), "db".to_string()];
        let edges = vec![edge("app", "db", "owns")];
        let order = topological_start_order(&ids, &edges);
        let db_pos = order.iter().position(|id| id == "db").unwrap();
        let app_pos = order.iter().position(|id| id == "app").unwrap();
        assert!(db_pos < app_pos);
    }

    #[test]
    fn topological_start_order_ignores_edges_outside_the_target_set() {
        // A flow-filtered run can exclude a node's dependency entirely —
        // that edge should just be ignored, not panic on a missing id.
        let ids = vec!["app".to_string()];
        let edges = vec![edge("app", "not-in-this-run", "depends-on")];
        let order = topological_start_order(&ids, &edges);
        assert_eq!(order, vec!["app".to_string()]);
    }

    #[test]
    fn topological_start_order_breaks_cycles_instead_of_looping_forever() {
        let ids = vec!["a".to_string(), "b".to_string()];
        let edges = vec![edge("a", "b", "depends-on"), edge("b", "a", "depends-on")];
        let order = topological_start_order(&ids, &edges);
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parse_env_file_skips_blanks_and_comments_and_strips_matching_quotes() {
        let contents =
            "# a comment\n\nFOO=bar\nBAZ=\"quoted value\"\nSINGLE='hi'\nMISMATCHED=\"oops'\n";
        let pairs = parse_env_file(contents);
        assert_eq!(
            pairs,
            vec![
                "FOO=bar",
                "BAZ=quoted value",
                "SINGLE=hi",
                "MISMATCHED=\"oops'",
            ]
        );
    }

    fn test_node(id: &str, label: &str, kind: &str) -> Node {
        Node {
            id: id.to_string(),
            label: label.to_string(),
            kind: kind.to_string(),
            image: None,
            branch: None,
            repo: None,
            domain_scope: "run".to_string(),
            local_path: None,
            domain: String::new(),
            downloaded: true,
            dirty: false,
            flows: Vec::new(),
            build: None,
            ports: BTreeMap::new(),
            environment: Vec::new(),
            command: Vec::new(),
            volumes: Vec::new(),
            additional_hosts: Vec::new(),
            wildcard_hosts: Vec::new(),
            env_file: Vec::new(),
            restart: "no".to_string(),
            user: None,
            working_dir: None,
            labels: BTreeMap::new(),
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            privileged: false,
            extra_hosts: Vec::new(),
            healthcheck: None,
            platform: None,
        }
    }

    fn test_graph(nodes: Vec<Node>, edges: Vec<Edge>) -> Graph {
        Graph {
            workspace_name: "shop".to_string(),
            nodes,
            edges,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn expand_service_fqdn_templates_resolves_self_reference() {
        let php = test_node("php.app", "php", "service");
        let graph = test_graph(vec![php.clone()], vec![]);
        let out = expand_service_fqdn_templates(
            "https://${FGHJ_SERVICE_FQDN}/",
            &php,
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(out, "https://php.app.shop.fghj.internal/");
    }

    #[test]
    fn expand_service_fqdn_templates_resolves_sibling_owned_by_a_service() {
        // php owns mysql; php's own environment references its sibling by name.
        let php = test_node("php.app", "php", "service");
        let mysql = test_node("mysql.php.app", "mysql", "backing");
        let graph = test_graph(
            vec![php.clone(), mysql],
            vec![edge("php.app", "mysql.php.app", "owns")],
        );
        let out = expand_service_fqdn_templates(
            "mysql://${FGHJ_SERVICE_FQDN:mysql}:3306/app",
            &php,
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(out, "mysql://mysql.php.app.shop.fghj.internal:3306/app");
    }

    #[test]
    fn expand_service_fqdn_templates_resolves_sibling_owned_by_the_same_owner() {
        // phpmyadmin and mysql are both owned by php; phpmyadmin references
        // its sibling mysql, not anything it owns itself (it owns nothing).
        let php_id = "php.app";
        let mysql = test_node("mysql.php.app", "mysql", "backing");
        let phpmyadmin = test_node("phpmyadmin.php.app", "phpmyadmin", "backing");
        let graph = test_graph(
            vec![mysql, phpmyadmin.clone()],
            vec![
                edge(php_id, "mysql.php.app", "owns"),
                edge(php_id, "phpmyadmin.php.app", "owns"),
            ],
        );
        let out = expand_service_fqdn_templates(
            "${FGHJ_SERVICE_FQDN:mysql}",
            &phpmyadmin,
            "phpmyadmin.php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(out, "mysql.php.app.shop.fghj.internal");
    }

    #[test]
    fn expand_service_fqdn_templates_resolves_a_directly_depended_on_sibling_service() {
        // vite depends on php (same-repo `kind: service`) — and the same
        // "depends-on" edge shape covers a cross-repo flow dependency, so
        // this also stands in for that case.
        let vite = test_node("vite.app", "vite", "service");
        let php = test_node("php.app", "php", "service");
        let graph = test_graph(
            vec![vite.clone(), php],
            vec![edge("vite.app", "php.app", "depends-on")],
        );
        let out = expand_service_fqdn_templates(
            "http://${FGHJ_SERVICE_FQDN:php}",
            &vite,
            "vite.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(out, "http://php.app.shop.fghj.internal");
    }

    #[test]
    fn expand_service_fqdn_templates_disambiguates_a_colliding_leaf_name_with_a_path() {
        // php owns a backing dependency named "mysql" *and* directly depends
        // on a cross-repo service that also happens to be named "mysql" —
        // the bare leaf name is ambiguous, so the backing dependency wins by
        // default (declared via "owns", checked first), and the qualified
        // root-first path (mirroring how the id itself, leaf-first, would
        // read as `mysql.otherrepo`) is needed to reach the other one.
        let php = test_node("php.app", "php", "service");
        let mysql_backing = test_node("mysql.php.app", "mysql", "backing");
        let mysql_service = test_node("mysql.otherrepo", "mysql", "service");
        let graph = test_graph(
            vec![php.clone(), mysql_backing, mysql_service],
            vec![
                edge("php.app", "mysql.php.app", "owns"),
                edge("php.app", "mysql.otherrepo", "depends-on"),
            ],
        );

        let bare = expand_service_fqdn_templates(
            "${FGHJ_SERVICE_FQDN:mysql}",
            &php,
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(bare, "mysql.php.app.shop.fghj.internal");

        let qualified = expand_service_fqdn_templates(
            "${FGHJ_SERVICE_FQDN:otherrepo::mysql}",
            &php,
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(qualified, "mysql.otherrepo.shop.fghj.internal");
    }

    #[test]
    fn expand_service_fqdn_templates_leaves_unknown_sibling_and_malformed_token_untouched() {
        let php = test_node("php.app", "php", "service");
        let graph = test_graph(vec![php.clone()], vec![]);

        let unknown = expand_service_fqdn_templates(
            "${FGHJ_SERVICE_FQDN:nope}",
            &php,
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(unknown, "${FGHJ_SERVICE_FQDN:nope}");

        let unterminated = expand_service_fqdn_templates(
            "prefix ${FGHJ_SERVICE_FQDN no closing brace",
            &php,
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(unterminated, "prefix ${FGHJ_SERVICE_FQDN no closing brace");
    }

    #[test]
    fn two_nodes_sharing_a_named_volume_derive_the_same_docker_name() {
        // Keyed by the declared `name`, not any node id — two unrelated
        // nodes (service or backing) that declare the same `name` + `scope`
        // land on the same derived value and therefore the same Docker volume.
        let a = derive_volume_name("cache", "run", "shop", "preview-1");
        let b = derive_volume_name("cache", "run", "shop", "preview-1");
        assert_eq!(a, b);

        // "stable" never folds in the run id, so it must differ from a
        // "run"-scoped name for the same non-default run.
        let stable = derive_volume_name("cache", "stable", "shop", "preview-1");
        assert_ne!(a, stable);

        // A different named run gets its own fresh "run"-scoped volume.
        let other_run = derive_volume_name("cache", "run", "shop", "preview-2");
        assert_ne!(a, other_run);
    }
}
