use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::docker;
use crate::resolver::{Graph, Node};
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
pub fn derive_domain(node_id: &str, domain_scope: &str, workspace_name: &str, run_id: &str) -> String {
    let workspace = sanitize_label(workspace_name);
    if domain_scope == "stable" || run_id == DEFAULT_RUN_ID {
        format!("{node_id}.{workspace}.fghj.internal")
    } else {
        format!("{node_id}.{run_id}.{workspace}.fghj.internal")
    }
}

/// A `*.fghj.internal` name this container answers to, and the `127.0.0.1`
/// port Docker actually published its backing container-side port on — the
/// SNI -> backend lookup `proxy::serve_https` dispatches real per-service
/// HTTPS routing through (see `WorkspaceRegistry::resolve_route`).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PortRoute {
    pub domain: String,
    pub host_port: u16,
}

#[derive(Debug, Serialize, Clone)]
pub struct ContainerInfo {
    pub node_id: String,
    pub container_name: String,
    pub status: String,
    pub published_port: Option<u16>,
    pub domain: String,
    pub routes: Vec<PortRoute>,
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
                    (run_id.clone(), state.containers.iter().map(|c| c.container_name.clone()).collect())
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
        docker::ensure_network(&self.docker, &network).await?;

        let owner = self.db.clone().load_owner().await.ok().flatten();

        let mut containers = Vec::new();
        for node in &graph.nodes {
            if node.kind == "flow" {
                continue;
            }
            if let Some(flow) = &spec.flow {
                if !node.flows.iter().any(|f| f == flow) {
                    continue;
                }
            }
            match self.start_node(graph, node, &run_id, &network, &spec.overrides, owner.as_ref()).await {
                Ok(info) => containers.push(info),
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
        docker::ensure_network(&self.docker, &network).await?;

        let mut state = self.runs.lock().unwrap().get(&run_id).cloned().unwrap_or_else(|| RunState {
            run_id: run_id.clone(),
            overrides: BTreeMap::new(),
            network: network.clone(),
            containers: Vec::new(),
        });

        let owner = self.db.clone().load_owner().await.ok().flatten();

        let targets: Vec<&Node> = graph
            .nodes
            .iter()
            .filter(|n| n.kind != "flow")
            .filter(|n| flow.is_none_or(|flow| n.flows.iter().any(|f| f == flow)))
            .collect();

        for node in targets {
            let container_name = format!("fghj-{}-{}-{}", sanitize_label(&graph.workspace_name), run_id, sanitize_label(&node.id));
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
                .start_node(graph, node, &run_id, &network, &BTreeMap::new(), owner.as_ref())
                .await?;
            state.containers.retain(|c| c.node_id != info.node_id);
            state.containers.push(info);
            // Saved after every node, not just at the end, so a later
            // failure in this same call doesn't lose track of containers
            // that did start successfully.
            self.db.clone().save_run(state.clone()).await?;
            self.runs.lock().unwrap().insert(run_id.clone(), state.clone());
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

        let image = match node.kind.as_str() {
            "backing" => match node.image.clone() {
                Some(img) => img,
                None => bail!("backing node {} has no image", node.id),
            },
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
                            crate::resolver::ensure_mirror(&repo, &mirror_dir, owner_for_mirror.as_ref())
                        })
                        .await
                        .context("ensure_mirror task panicked")??;
                        let checkout = internal_dir.join("checkouts").join(format!(
                            "{}-{}",
                            sanitize_label(&node.id),
                            sanitize_label(branch)
                        ));
                        let build_dir =
                            docker::materialize_checkout(&mirror, branch, &checkout).await?
                                .join(&build.context);
                        docker::build_image(&self.docker, &build_dir, &build.dockerfile, &tag).await?;
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
                        let build_dir = self.workspace.join(&local_path).join(&build.context);
                        docker::build_image(&self.docker, &build_dir, &build.dockerfile, &tag).await?;
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
        aliases.extend(node.ports.values().filter_map(|p| p.name.as_ref()).map(|name| format!("{name}.{domain}")));

        let port_list: Vec<(String, Option<u16>)> =
            node.ports.iter().map(|(port, cfg)| (port.clone(), cfg.host_port)).collect();

        docker::run_container(
            &self.docker,
            &docker::RunOpts {
                name: &container_name,
                network,
                aliases: &aliases,
                env: &node.environment,
                ports: &port_list,
                image: &image,
                project: network,
                service_name: &node.id,
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

        // Every port with a domain — the `primary` one, at this node's own
        // domain, and/or any `name`d one, at `{name}.{domain}` (a port can
        // be both) — gets a route to the host port Docker actually
        // published it on. `fghjd` runs on the host, not inside the docker
        // network, so it can't resolve these names the way sibling
        // containers do (via Docker's embedded per-network DNS, which only
        // answers from inside that network) — this is what lets
        // `proxy::serve_https` dispatch an incoming SNI straight to the
        // right container instead. Reuses the inspect already done above
        // for `status_port`'s own binding rather than re-inspecting it.
        let mut routes = Vec::new();
        for (port, cfg) in &node.ports {
            if !cfg.primary && cfg.name.is_none() {
                continue;
            }
            let host_port = if status_port.as_deref() == Some(port.as_str()) {
                published_port
            } else {
                docker::inspect_status(&self.docker, &container_name, port)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|s| s.published_port)
            };
            let Some(host_port) = host_port else { continue };
            if cfg.primary {
                routes.push(PortRoute { domain: domain.clone(), host_port });
            }
            if let Some(name) = &cfg.name {
                routes.push(PortRoute { domain: format!("{name}.{domain}"), host_port });
            }
        }

        Ok(ContainerInfo {
            node_id: node.id.clone(),
            container_name,
            status,
            published_port,
            domain,
            routes,
        })
    }
}

pub async fn logs_for_tail(docker: &bollard::Docker, state: &RunState, node_id: &str, tail: usize) -> Result<String> {
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
        assert_eq!(sanitize_label("Feature/JIRA-123 Fix"), "feature-jira-123-fix");
        assert_eq!(sanitize_label("already-clean"), "already-clean");
        assert_eq!(sanitize_label("__leading__"), "leading");
    }
}
