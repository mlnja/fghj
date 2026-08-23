use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
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
}

#[derive(Debug, Serialize, Clone)]
pub struct ContainerInfo {
    pub node_id: String,
    pub container_name: String,
    pub status: String,
    pub published_port: Option<u16>,
    pub domain: String,
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
    runs: Mutex<BTreeMap<String, RunState>>,
}

impl RunRegistry {
    /// Loads any runs persisted from a previous `fghjd` lifetime and
    /// reconciles each against real docker state: a run whose containers are
    /// all still alive is restored with freshly-inspected statuses, and a run
    /// missing any container (removed out-of-band, or lost across a reboot
    /// with no restart policy) is dropped rather than presented as running.
    pub fn new(workspace: std::path::PathBuf, db: Arc<WorkspaceDb>) -> Result<Self> {
        let persisted = db.load_runs()?;
        let mut reconciled = BTreeMap::new();
        for (run_id, mut state) in persisted {
            let mut alive = true;
            for c in &mut state.containers {
                match docker::inspect_status(&c.container_name, "") {
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
                let _ = db.delete_run(&run_id);
            }
        }
        Ok(Self {
            workspace,
            db,
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
    pub fn refresh(&self) {
        let mut runs = self.runs.lock().unwrap();
        for state in runs.values_mut() {
            let mut changed = false;
            for c in &mut state.containers {
                let status = match docker::inspect_status(&c.container_name, "") {
                    Ok(Some(s)) => s.status,
                    _ => "removed".to_string(),
                };
                if status != c.status {
                    c.status = status;
                    changed = true;
                }
            }
            if changed {
                let _ = self.db.save_run(state);
            }
        }
    }

    pub fn get(&self, run_id: &str) -> Option<RunState> {
        self.runs.lock().unwrap().get(run_id).cloned()
    }

    pub fn stop(&self, run_id: &str) -> Result<()> {
        let mut runs = self.runs.lock().unwrap();
        let Some(state) = runs.remove(run_id) else {
            bail!("no such run: {run_id}");
        };
        for c in &state.containers {
            docker::stop_and_remove(&c.container_name)?;
        }
        docker::remove_network(&state.network)?;
        self.db.delete_run(run_id)?;
        Ok(())
    }

    pub fn start(&self, graph: &Graph, spec: RunSpec) -> Result<RunState> {
        let run_id = spec
            .run_id
            .as_deref()
            .map(sanitize_label)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_RUN_ID.to_string());

        // starting an already-running run replaces it cleanly
        if self.runs.lock().unwrap().contains_key(&run_id) {
            self.stop(&run_id)?;
        }

        let network = format!("fghj-{}-{}", sanitize_label(&graph.workspace_name), run_id);
        docker::ensure_network(&network)?;

        let mut containers = Vec::new();
        for node in &graph.nodes {
            if node.kind == "flow" {
                continue;
            }
            match self.start_node(graph, node, &run_id, &network, &spec.overrides) {
                Ok(info) => containers.push(info),
                Err(e) => {
                    for c in &containers {
                        let _ = docker::stop_and_remove(&c.container_name);
                    }
                    docker::remove_network(&network)?;
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
        self.db.save_run(&state)?;
        self.runs.lock().unwrap().insert(run_id, state.clone());
        Ok(state)
    }

    fn start_node(
        &self,
        graph: &Graph,
        node: &Node,
        run_id: &str,
        network: &str,
        overrides: &BTreeMap<String, String>,
    ) -> Result<ContainerInfo> {
        let workspace = sanitize_label(&graph.workspace_name);
        let container_name = format!("fghj-{workspace}-{run_id}-{}", sanitize_label(&node.id));
        let domain = if run_id == DEFAULT_RUN_ID {
            format!("{}.{}.fghj", node.label, workspace)
        } else {
            format!("{}.{}.{}.fghj", node.label, run_id, workspace)
        };

        let image = match node.kind.as_str() {
            "infra" => match node.image.clone() {
                Some(img) => img,
                None => bail!("infra node {} has no image", node.id),
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
                        let mirror = crate::resolver::ensure_mirror(&repo, &internal_dir)?;
                        let checkout = internal_dir.join("checkouts").join(format!(
                            "{}-{}",
                            sanitize_label(&node.id),
                            sanitize_label(branch)
                        ));
                        let build_dir =
                            docker::materialize_checkout(&mirror, branch, &checkout)?
                                .join(&build.context);
                        docker::build_image(&build_dir, &build.dockerfile, &tag)?;
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
                        docker::build_image(&build_dir, &build.dockerfile, &tag)?;
                        tag
                    }
                }
            }
        };

        let aliases = vec![node.domain.clone().unwrap_or_default(), domain.clone()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();

        docker::run_container(&docker::RunOpts {
            name: &container_name,
            network,
            aliases: &aliases,
            env: &node.environment,
            ports: &node.ports,
            image: &image,
            project: network,
            service_name: &node.id,
        })?;

        let first_port = node.ports.first().map(|p| p.split('/').next().unwrap_or(p).to_string());
        let inspected = match &first_port {
            Some(p) => docker::inspect_status(&container_name, p)?,
            None => docker::inspect_status(&container_name, "")?,
        };
        let (status, published_port) = match inspected {
            Some(s) => (s.status, s.published_port),
            None => ("unknown".to_string(), None),
        };

        Ok(ContainerInfo {
            node_id: node.id.clone(),
            container_name,
            status,
            published_port,
            domain,
        })
    }
}

pub fn logs_for(state: &RunState, node_id: &str, tail: usize) -> Result<String> {
    let Some(c) = state.containers.iter().find(|c| c.node_id == node_id) else {
        bail!("no such node in run: {node_id}");
    };
    docker::logs(&c.container_name, tail)
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
