//! `ResolveCtx` — turns every repo's config into nodes and edges, in phases
//! that each need the previous one complete for every repo. Flows are expanded
//! afterwards in [`super::flows`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use super::config::{
    ComponentConfig, default_restart, default_run_policy, default_stop_grace_period,
};
use super::dependency::{Condition, Target};
use super::git::{git_head_sha, git_remote_and_branch, git_status_dirty};
use super::graph::{BuildSource, Edge, Node, NodeBuild, NodeBuildSecret};
use super::repo_url::{normalize_repo_url, repo_name_from_url};
use super::warning::Warning;

pub struct ResolveCtx<'a> {
    pub(crate) workspace: &'a Path,
    pub(crate) scanned: &'a BTreeMap<String, ComponentConfig>,
    /// Keyed by the real git remote, not the folder name.
    pub(crate) repo_index: HashMap<String, String>,
    /// Shared, so two repos including the same missing one get one stub.
    pub(crate) stub_repo_ids: HashMap<String, String>,
    pub(crate) includes: BTreeMap<String, BTreeMap<String, String>>,
    pub(crate) nodes: HashMap<String, Node>,
    pub(crate) edges: Vec<Edge>,
    pub(crate) warnings: Vec<Warning>,
}

/// Leaf-first, like the domain built from it.
pub(crate) fn node_id(name: &str, local_path: &str) -> String {
    format!("{name}.{local_path}")
}

impl<'a> ResolveCtx<'a> {
    pub(crate) fn new(
        workspace: &'a Path,
        scanned: &'a BTreeMap<String, ComponentConfig>,
        repo_index: HashMap<String, String>,
    ) -> Self {
        ResolveCtx {
            workspace,
            scanned,
            repo_index,
            stub_repo_ids: HashMap::new(),
            includes: BTreeMap::new(),
            nodes: HashMap::new(),
            edges: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub(crate) fn is_scanned(&self, local_path: &str) -> bool {
        self.scanned.contains_key(local_path)
    }

    /// Phase 1. `build` makes `"service"`, `image` only `"backing"` — until
    /// [`Self::mark_tasks`] says otherwise.
    pub(crate) fn add_repo_nodes(&mut self, local_path: &str, component: &ComponentConfig) {
        if component.services.is_empty() {
            self.warnings.push(Warning::advisory(format!(
                "'{local_path}' declares no services"
            )));
        }
        let dir = self.workspace.join(local_path);
        let (repo, branch) = git_remote_and_branch(&dir);
        let dirty = git_status_dirty(&dir);
        let head = git_head_sha(&dir);

        for (name, service) in &component.services {
            let id = node_id(name, local_path);
            let kind = match (&service.build, &service.image) {
                (Some(_), None) => "service",
                (None, Some(_)) => "backing",
                (Some(_), Some(_)) => {
                    self.warnings.push(Warning::blocking(format!(
                        "'{id}' sets both `build` and `image` — fghj names the images it builds \
                         itself, so set one: `build` for your code, `image` for a published one"
                    )));
                    "service"
                }
                (None, None) => {
                    self.warnings.push(Warning::blocking(format!(
                        "'{id}' sets neither `build` nor `image`, so there is nothing to run"
                    )));
                    "service"
                }
            };
            let ports = service.ports.clone().into_map();
            self.check_ports(&id, service, &ports);
            let remote = service
                .build
                .as_ref()
                .and_then(|b| self.check_build(&id, b));
            self.nodes.insert(
                id.clone(),
                Node {
                    id: id.clone(),
                    label: name.to_string(),
                    kind: kind.into(),
                    image: service.image.clone(),
                    branch: branch.clone(),
                    repo: repo.clone(),
                    local_path: Some(local_path.to_string()),
                    domain: String::new(),
                    downloaded: true,
                    dirty,
                    head: head.clone(),
                    flows: Vec::new(),
                    includes: BTreeMap::new(),
                    build: service.build.as_ref().map(|b| NodeBuild {
                        context: remote
                            .as_ref()
                            .map(|r| r.subdir.clone().unwrap_or_else(|| ".".into()))
                            .unwrap_or_else(|| b.context.clone()),
                        dockerfile: b.dockerfile.clone(),
                        dockerfile_inline: b.dockerfile_inline.clone(),
                        source: remote
                            .as_ref()
                            .map(|r| BuildSource::read(self.workspace, r)),
                        args: b.args.clone(),
                        target: b.target.clone(),
                        ssh: b.ssh,
                        secrets: b
                            .secrets
                            .iter()
                            .map(|s| NodeBuildSecret {
                                id: s.id.clone(),
                                file: s.file.clone(),
                            })
                            .collect(),
                    }),
                    ports,
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
                    restart: service.restart_or_default(),
                    stop_signal: service.stop_signal.clone(),
                    stop_grace_period: service.stop_grace_period,
                    user: service.user.clone(),
                    working_dir: service.working_dir.clone(),
                    labels: service.labels.clone(),
                    cap_add: service.cap_add.clone(),
                    cap_drop: service.cap_drop.clone(),
                    privileged: service.privileged,
                    extra_hosts: service.extra_hosts.clone(),
                    healthcheck: service.healthcheck.clone(),
                    platform: service.platform.clone(),
                    run_policy: None,
                    migrates: None,
                    seeds: Vec::new(),
                    debug: service.debug,
                },
            );
        }
    }

    /// Phase 2. By the real git remote if on disk under another name, else by the
    /// folder the URL clones to; a stub otherwise. Never clones.
    pub(crate) fn add_includes(&mut self, local_path: &str, component: &ComponentConfig) {
        let mut resolved = BTreeMap::new();
        for (alias, include) in &component.include {
            if component.services.contains_key(alias.as_str())
                || component.flows.contains_key(alias.as_str())
            {
                self.warnings.push(Warning::blocking(format!(
                    "'{local_path}' uses '{alias}' both as an include alias and as a service or \
                     flow name, so a bare '{alias}' could mean either — rename one"
                )));
            }
            let repo = include.repo();
            let norm = normalize_repo_url(repo);
            let target = self
                .repo_index
                .get(&norm)
                .cloned()
                .unwrap_or_else(|| repo_name_from_url(repo));
            let target = if self.is_scanned(&target) {
                target
            } else {
                self.stub(&norm, target, repo, include.default_branch())
            };
            resolved.insert(alias.to_string(), target);
        }
        for flow in component.flows.keys() {
            if component.services.contains_key(flow.as_str()) {
                self.warnings.push(Warning::blocking(format!(
                    "'{local_path}' has a service and a flow both named '{flow}', so a bare \
                     '{flow}' in a flow could mean either — rename one"
                )));
            }
        }
        for node in self.nodes.values_mut() {
            if node.local_path.as_deref() == Some(local_path) && node.downloaded {
                node.includes = resolved.clone();
            }
        }
        self.includes.insert(local_path.to_string(), resolved);
    }

    fn stub(
        &mut self,
        norm: &str,
        local_path: String,
        repo: &str,
        default_branch: Option<&str>,
    ) -> String {
        let stub_id = self
            .stub_repo_ids
            .entry(norm.to_string())
            .or_insert(local_path)
            .clone();
        self.nodes.entry(stub_id.clone()).or_insert_with(|| Node {
            id: stub_id.clone(),
            label: stub_id.clone(),
            kind: "service".into(),
            image: None,
            branch: default_branch.map(str::to_string),
            repo: Some(repo.to_string()),
            local_path: Some(stub_id.clone()),
            domain: String::new(),
            downloaded: false,
            dirty: false,
            head: None,
            flows: Vec::new(),
            includes: BTreeMap::new(),
            build: None,
            ports: BTreeMap::new(),
            environment: Vec::new(),
            command: Vec::new(),
            volumes: Vec::new(),
            additional_hosts: Vec::new(),
            wildcard_hosts: Vec::new(),
            env_file: Vec::new(),
            restart: default_restart(),
            stop_signal: None,
            stop_grace_period: default_stop_grace_period(),
            user: None,
            working_dir: None,
            labels: BTreeMap::new(),
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            privileged: false,
            extra_hosts: Vec::new(),
            healthcheck: None,
            platform: None,
            run_policy: None,
            migrates: None,
            seeds: Vec::new(),
            debug: None,
        });
        stub_id
    }

    /// Phase 3. Waited on with `service_completed_successfully`, or declares
    /// `run:`, `migrates:` or `seeds:`. A kind, not an edge condition, because "exited" is success for a
    /// task and drift for anything else.
    pub(crate) fn mark_tasks(&mut self, local_path: &str, component: &ComponentConfig) {
        let mut tasks: BTreeSet<&str> = BTreeSet::new();
        for service in component.services.values() {
            for (target, entry) in service.depends_on.entries() {
                if entry.condition == Condition::ServiceCompletedSuccessfully
                    && let Some((name, _)) = component.services.get_key_value(target.as_str())
                {
                    tasks.insert(name.as_str());
                }
            }
        }
        for (name, service) in &component.services {
            let id = node_id(name, local_path);
            let store_task = service.migrates.is_some() || !service.seeds.is_empty();
            if !tasks.contains(name.as_str()) && service.run.is_none() && !store_task {
                continue;
            }
            // An exited container can never be healthy, and a restart policy would
            // restart it forever. Refused so the file never reads as honoured.
            if service.healthcheck.is_some() {
                self.warnings.push(Warning::blocking(format!(
                    "task '{id}' declares a healthcheck — a task is done when it exits 0, \
                     and an exited container can never report healthy"
                )));
            }
            if service.restart.as_deref().is_some_and(|r| r != "no") {
                self.warnings.push(Warning::blocking(format!(
                    "task '{id}' sets `restart` — a container whose purpose is to exit \
                     would be restarted forever; leave it unset"
                )));
            }
            // Otherwise it runs the image's long-running `CMD` and never exits.
            if service.command.is_empty() {
                self.warnings.push(Warning::blocking(format!(
                    "task '{id}' declares no `command` — a task is its command, and one \
                     without would run the image's default CMD and never exit"
                )));
            }
            let run = service.run.clone().unwrap_or_else(default_run_policy);
            if run != "on_start" && run != "once" {
                self.warnings.push(Warning::blocking(format!(
                    "task '{id}' sets `run: {run}` — it is \"on_start\" or \"once\""
                )));
            }
            let (migrates, seeds) = self.check_stores(local_path, component, name, service);
            if let Some(node) = self.nodes.get_mut(&id) {
                node.kind = "task".into();
                node.run_policy = Some(run);
                node.healthcheck = None;
                node.migrates = migrates;
                node.seeds = seeds;
            }
        }
    }

    /// `migrates:` and `seeds:` name services in this repo; what's kept is
    /// their node ids, minus any that name nothing.
    fn check_stores(
        &mut self,
        local_path: &str,
        component: &ComponentConfig,
        name: &str,
        service: &super::service::ServiceConfig,
    ) -> (Option<String>, Vec<String>) {
        let id = node_id(name, local_path);
        let (migrates, seeds) = (service.migrates.as_ref(), &service.seeds);
        if migrates.is_none() && seeds.is_empty() {
            return (None, Vec::new());
        }
        if service.run.is_some() {
            self.warnings.push(Warning::blocking(format!(
                "'{id}' sets `run` alongside `{}` — that decides when it runs; drop `run`",
                if migrates.is_some() {
                    "migrates"
                } else {
                    "seeds"
                }
            )));
        }
        if migrates.is_some() && !seeds.is_empty() {
            self.warnings.push(Warning::blocking(format!(
                "'{id}' sets both `migrates` and `seeds` — a migration runs on every start and \
                 a seed only on new data, so split it into two services"
            )));
        }
        let mut store = |field: &str, store: &str| -> Option<String> {
            if store == name {
                self.warnings.push(Warning::blocking(format!(
                    "'{id}' names itself in `{field}`"
                )));
                return None;
            }
            if !component.services.contains_key(store) {
                self.warnings.push(Warning::blocking(format!(
                    "'{id}' {field} '{store}', but '{local_path}' has no service by that name \
                     — a store is in the same repo as what migrates or seeds it"
                )));
                return None;
            }
            Some(node_id(store, local_path))
        };
        let migrates = migrates.and_then(|m| store("migrates", m));
        let seeds = seeds.iter().filter_map(|s| store("seeds", s)).collect();
        (migrates, seeds)
    }

    /// Phase 5, once every repo has its tasks and edges. A migration or seed
    /// waits on its stores; whatever waits on a store waits on its migration.
    pub(crate) fn add_store_edges(&mut self) {
        let mut tasks: Vec<(String, Vec<String>)> = self
            .nodes
            .values()
            .filter(|n| n.migrates.is_some() || !n.seeds.is_empty())
            .map(|n| {
                (
                    n.id.clone(),
                    n.migrates.iter().chain(&n.seeds).cloned().collect(),
                )
            })
            .collect();
        tasks.sort();

        let mut migrations: BTreeMap<String, String> = BTreeMap::new();
        for (task, stores) in &tasks {
            for store in stores {
                let Some(target) = self.nodes.get(store) else {
                    continue;
                };
                if target.kind == "task" {
                    self.warnings.push(Warning::blocking(format!(
                        "'{task}' names '{store}' as a store, but '{store}' is a task — a store \
                         is something that stays up"
                    )));
                    continue;
                }
                let condition = if target.healthcheck.is_some() {
                    Condition::ServiceHealthy
                } else {
                    Condition::ServiceStarted
                };
                if !self.edges.iter().any(|e| e.from == *task && e.to == *store) {
                    self.push_dependency(task, store.clone(), true, condition, None);
                }
            }
            if let Some(store) = self.nodes[task].migrates.clone()
                && let Some(other) = migrations.insert(store.clone(), task.clone())
            {
                self.warnings.push(Warning::blocking(format!(
                    "'{other}' and '{task}' both migrate '{store}' — one migration per \
                     store, so their order can't be a guess"
                )));
            }
        }

        let mut implied = Vec::new();
        let mut seen = BTreeSet::new();
        for edge in self.edges.iter().filter(|e| e.needed_to_start()) {
            let Some(migration) = migrations.get(&edge.to) else {
                continue;
            };
            if edge.from == *migration
                || self
                    .edges
                    .iter()
                    .any(|e| e.from == edge.from && e.to == *migration)
                || !seen.insert((edge.from.clone(), migration.clone()))
            {
                continue;
            }
            implied.push(Edge {
                from: edge.from.clone(),
                to: migration.clone(),
                kind: "depends-on".into(),
                required: true,
                condition: Some(Condition::ServiceCompletedSuccessfully.as_str().into()),
                via_flow: edge.via_flow.clone(),
                flows: Vec::new(),
                implied: true,
            });
        }
        self.edges.extend(implied);
    }

    /// Phase 4. Across repos a key names a flow or the whole repo, never a
    /// service. A wait on a flow becomes one edge per member.
    pub(crate) fn add_dependency_edges(&mut self, local_path: &str, component: &ComponentConfig) {
        for (name, service) in &component.services {
            let from = node_id(name, local_path);
            for (key, entry) in service.depends_on.entries() {
                let condition = entry.condition;
                match Target::parse(&key) {
                    Target::Bare(target) if component.services.contains_key(target) => {
                        let to = node_id(target, local_path);
                        if to == from {
                            self.warnings
                                .push(Warning::blocking(format!("'{from}' depends on itself")));
                            continue;
                        }
                        if entry.required {
                            self.check_condition(&from, &to, condition);
                        } else {
                            self.check_runtime_condition(&from, &key, condition);
                        }
                        self.push_dependency(&from, to, entry.required, condition, None);
                    }
                    Target::Bare(target) if component.flows.contains_key(target) => {
                        self.warnings.push(Warning::blocking(format!(
                            "'{from}' depends on '{target}', which is a flow in this repo — \
                             within a repo, `depends_on` names services"
                        )));
                    }
                    Target::Bare(alias) | Target::Qualified { alias, .. }
                        if !self.has_alias(local_path, alias) =>
                    {
                        self.warnings.push(Warning::blocking(format!(
                            "'{from}' depends on '{key}', but '{local_path}' has no service and \
                             no include named '{alias}'"
                        )));
                    }
                    target => {
                        if condition == Condition::ServiceCompletedSuccessfully {
                            self.warnings.push(Warning::blocking(format!(
                                "'{from}' waits on '{key}' with \
                                 `condition: service_completed_successfully` — another repo's \
                                 flow is waited on until it is up, it never completes"
                            )));
                            continue;
                        }
                        if !entry.required {
                            self.check_runtime_condition(&from, &key, condition);
                        }
                        let members = match self.foreign_members(local_path, target) {
                            Ok(members) => members,
                            Err(e) => {
                                self.warnings
                                    .push(Warning::blocking(format!("'{from}' {e}")));
                                continue;
                            }
                        };
                        let via = self.flow_label(local_path, target);
                        for to in members {
                            self.push_dependency(
                                &from,
                                to,
                                entry.required,
                                condition,
                                Some(via.clone()),
                            );
                        }
                    }
                }
            }
        }
    }

    fn push_dependency(
        &mut self,
        from: &str,
        to: String,
        required: bool,
        condition: Condition,
        via_flow: Option<String>,
    ) {
        self.edges.push(Edge {
            from: from.to_string(),
            to,
            kind: "depends-on".into(),
            required,
            condition: Some(condition.as_str().into()),
            via_flow,
            flows: Vec::new(),
            implied: false,
        });
    }

    fn check_condition(&mut self, from: &str, to: &str, condition: Condition) {
        let Some(target) = self.nodes.get(to) else {
            return;
        };
        let is_task = target.kind == "task";
        let problem = match condition {
            Condition::ServiceCompletedSuccessfully => None,
            _ if is_task => Some(format!(
                "'{from}' waits on '{to}' with `{}`, but something else waits on it with \
                 `service_completed_successfully`, which makes it a task — wait on it that way \
                 everywhere",
                condition.as_str()
            )),
            Condition::ServiceHealthy if target.healthcheck.is_none() => Some(format!(
                "'{from}' waits on '{to}' with `service_healthy`, but '{to}' declares no \
                 healthcheck"
            )),
            _ => None,
        };
        if let Some(problem) = problem {
            self.warnings.push(Warning::blocking(problem));
        }
    }

    /// `service_completed_successfully` contradicts not being needed to start;
    /// the other two are Compose syntax fghj can't honour here, said rather than
    /// dropped.
    fn check_runtime_condition(&mut self, from: &str, key: &str, condition: Condition) {
        match condition {
            Condition::ServiceStarted => {}
            Condition::ServiceCompletedSuccessfully => {
                self.warnings.push(Warning::blocking(format!(
                    "'{from}' waits on '{key}' with `service_completed_successfully` but marks \
                     it `required: false` — a task has to finish before its dependent \
                     starts, so it is needed to start; drop `required: false`, or give the \
                     task `run:` and list it in a flow instead"
                )));
            }
            Condition::ServiceHealthy => {
                self.warnings.push(Warning::advisory(format!(
                    "'{from}' waits on '{key}' with `service_healthy`, which is ignored: \
                     it's `required: false`, and nothing waits on a runtime dependency"
                )));
            }
        }
    }

    pub(crate) fn has_alias(&self, local_path: &str, alias: &str) -> bool {
        self.includes
            .get(local_path)
            .is_some_and(|m| m.contains_key(alias))
    }

    pub(crate) fn alias_target(&self, local_path: &str, alias: &str) -> Option<&str> {
        self.includes
            .get(local_path)
            .and_then(|m| m.get(alias))
            .map(String::as_str)
    }

    /// The alias replaced by its folder: two repos can alias one repo differently.
    pub(crate) fn flow_label(&self, local_path: &str, target: Target<'_>) -> String {
        match target {
            Target::Bare(alias) => self
                .alias_target(local_path, alias)
                .unwrap_or(alias)
                .to_string(),
            Target::Qualified { alias, name } => format!(
                "{}/{name}",
                self.alias_target(local_path, alias).unwrap_or(alias)
            ),
        }
    }

    /// Phase 6. Every `${FGHJ_SERVICE_FQDN:…}` must name a service. Another
    /// repo's *service* is allowed here and nowhere else. No edge comes of it:
    /// see `concepts/dependency-kinds.md`.
    pub(crate) fn check_references(&mut self) {
        let mut ids: Vec<&String> = self.nodes.keys().collect();
        ids.sort();
        for id in ids {
            let node = &self.nodes[id];
            if !node.downloaded {
                continue;
            }
            let Some(local_path) = node.local_path.as_deref() else {
                continue;
            };
            let mut seen = BTreeSet::new();
            for pair in &node.environment {
                for path in crate::orchestrator::fqdn_template::fqdn_template_paths(pair) {
                    if !seen.insert(path.clone()) {
                        continue;
                    }
                    let Some(to) = reference_target_id(local_path, &node.includes, &path) else {
                        self.warnings.push(Warning::blocking(format!(
                            "'{id}' references '{path}' in a ${{FGHJ_SERVICE_FQDN:…}} template, \
                             but '{local_path}' has no service and no include named '{}'",
                            path.split('/').next().unwrap_or(&path)
                        )));
                        continue;
                    };
                    match self.nodes.get(&to) {
                        Some(_) => {}
                        None if target_repo_is_stub(self, &node.includes, &path) => {}
                        None => self.warnings.push(Warning::blocking(format!(
                            "'{id}' references '{path}' in a ${{FGHJ_SERVICE_FQDN:…}} template, \
                             but there is no such service"
                        ))),
                    }
                }
            }
        }
    }
}

fn target_repo_is_stub(
    ctx: &ResolveCtx<'_>,
    includes: &BTreeMap<String, String>,
    path: &str,
) -> bool {
    match Target::parse(path) {
        Target::Bare(_) => false,
        Target::Qualified { alias, .. } => includes
            .get(alias)
            .is_some_and(|repo| !ctx.is_scanned(repo)),
    }
}

pub(crate) fn reference_target_id(
    local_path: &str,
    includes: &BTreeMap<String, String>,
    path: &str,
) -> Option<String> {
    match Target::parse(path) {
        Target::Bare(name) => Some(node_id(name, local_path)),
        Target::Qualified { alias, name } => includes.get(alias).map(|t| node_id(name, t)),
    }
}
