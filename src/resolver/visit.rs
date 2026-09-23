//! `ResolveCtx` — the traversal that walks components and their
//! dependencies, turning parsed config into graph nodes and edges.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use super::config::ComponentConfig;
use super::git::{git_remote_and_branch, git_status_dirty};
use super::graph::{Edge, Node, NodeBuild};

pub struct ResolveCtx<'a> {
    pub(crate) workspace: &'a Path,
    pub(crate) scanned: &'a BTreeMap<String, ComponentConfig>,
    /// normalized repo url -> local_path, for every repo actually on disk
    /// (keyed by its real git remote, not by whatever local_path a given
    /// dependent happens to guess/override).
    pub(crate) repo_index: HashMap<String, String>,
    /// normalized repo url -> stub node id, for repos not yet on disk. Ties
    /// together dependents that reference the same repo via different
    /// `local_path` spellings so they don't render as separate nodes.
    pub(crate) stub_repo_ids: HashMap<String, String>,
    pub(crate) nodes: HashMap<String, Node>,
    pub(crate) edges: Vec<Edge>,
    /// local paths already fully expanded, to avoid re-walking / infinite loops.
    pub(crate) visited: HashSet<String>,
    pub(crate) warnings: Vec<String>,
}

impl<'a> ResolveCtx<'a> {
    /// Registers (and, unless already visited, recursively expands) every
    /// service node declared by a repo already present on disk at
    /// `local_path`. Returns the id of each, keyed by its name in
    /// `component.services` — cheap to call again for an already-visited
    /// repo (just returns the same ids without re-walking dependencies).
    pub(crate) fn visit_local_services(
        &mut self,
        local_path: &str,
        component: &ComponentConfig,
    ) -> BTreeMap<String, String> {
        // Leaf-first, qualified by the repo's own workspace folder name — a
        // service name (e.g. "bff") is only a friendly label, not a unique
        // id: two peer repos (no ownership relation between them, per
        // [[flat-workspace-model]]) can legitimately declare the same one.
        // `local_path` is the one thing guaranteed unique per repo (it's a
        // real folder name — `scan_workspace` can't have two), so folding it
        // in always (not just when a collision is actually detected) means
        // adding a same-named peer repo later can never silently rehost an
        // existing one's domain out from under it.
        let ids: BTreeMap<String, String> = component
            .services
            .keys()
            .map(|name| (name.clone(), format!("{name}.{local_path}")))
            .collect();

        for (name, service_id) in &ids {
            let service = &component.services[name];
            self.nodes.entry(service_id.clone()).or_insert_with(|| {
                let dir = self.workspace.join(local_path);
                let (repo, branch) = git_remote_and_branch(&dir);
                let dirty = git_status_dirty(&dir);
                Node {
                    id: service_id.clone(),
                    label: name.clone(),
                    kind: "service".into(),
                    image: None,
                    branch,
                    repo,
                    domain_scope: service.domain_scope.clone(),
                    local_path: Some(local_path.to_string()),
                    domain: String::new(),
                    downloaded: true,
                    dirty,
                    flows: Vec::new(),
                    build: service.build.as_ref().map(|b| NodeBuild {
                        context: b.context.clone(),
                        dockerfile: b.dockerfile.clone(),
                        args: b.args.clone(),
                    }),
                    ports: service.ports.clone(),
                    environment: service.environment.to_pairs(),
                    command: service.command.clone(),
                    volumes: service.volumes.clone(),
                    additional_hosts: service
                        .additional_hosts
                        .iter()
                        .filter(|h| !h.wildcard())
                        .map(|h| h.host().to_string())
                        .collect(),
                    wildcard_hosts: service
                        .additional_hosts
                        .iter()
                        .filter(|h| h.wildcard())
                        .map(|h| h.host().to_string())
                        .collect(),
                    env_file: service.env_file.clone(),
                    restart: service.restart.clone(),
                    user: service.user.clone(),
                    working_dir: service.working_dir.clone(),
                    labels: service.labels.clone(),
                    cap_add: service.cap_add.clone(),
                    cap_drop: service.cap_drop.clone(),
                    privileged: service.privileged,
                    extra_hosts: service.extra_hosts.clone(),
                    healthcheck: service.healthcheck.clone(),
                    platform: service.platform.clone(),
                }
            });
        }

        if !self.visited.insert(local_path.to_string()) {
            return ids;
        }

        for (name, service_id) in &ids {
            let service = &component.services[name];
            self.check_ports(service_id, service);
            for dep in service.dependencies.clone() {
                self.visit_dependency(service_id, local_path, dep);
            }
        }

        ids
    }

    /// Picks one service id out of `visit_local_services`' result: `wanted`
    /// if given, or the sole entry if the repo declares exactly one and
    /// `wanted` is `None`. Pushes a (non-fatal) warning and returns `None`
    /// if that's ambiguous (multiple services, no `wanted`), the repo
    /// declares none at all, or `wanted` names one that doesn't exist —
    /// same "warn, don't panic" style as `check_ports`.
    pub(crate) fn visit_local_service(
        &mut self,
        local_path: &str,
        component: &ComponentConfig,
        wanted: Option<&str>,
    ) -> Option<String> {
        let ids = self.visit_local_services(local_path, component);
        let name = match wanted {
            Some(name) => name.to_string(),
            None => match ids.len() {
                1 => return ids.into_values().next(),
                0 => {
                    self.warnings
                        .push(format!("'{local_path}' declares no services"));
                    return None;
                }
                _ => {
                    self.warnings.push(format!(
                        "'{local_path}' declares multiple services ({}); specify which one with `service:`",
                        ids.keys().cloned().collect::<Vec<_>>().join(", ")
                    ));
                    return None;
                }
            },
        };
        match ids.get(&name) {
            Some(id) => Some(id.clone()),
            None => {
                self.warnings
                    .push(format!("'{local_path}' has no service named '{name}'"));
                None
            }
        }
    }
}
