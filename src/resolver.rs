use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Clone)]
struct FlowConfig {
    #[allow(dead_code)]
    description: Option<String>,
    dependencies: Vec<Dependency>,
}

#[derive(Debug, Deserialize, Clone)]
struct ComponentConfig {
    #[allow(dead_code)]
    version: String,
    service: ServiceConfig,
    #[serde(default)]
    flows: BTreeMap<String, FlowConfig>,
}

#[derive(Debug, Deserialize, Clone)]
struct Build {
    #[serde(default = "default_context")]
    context: String,
    #[serde(default = "default_dockerfile")]
    dockerfile: String,
    #[serde(default)]
    args: BTreeMap<String, String>,
}

fn default_context() -> String {
    ".".to_string()
}

fn default_dockerfile() -> String {
    "Dockerfile".to_string()
}

/// Mirrors `#Environment` in `schema/dependency.cue`: Compose accepts `environment`
/// as either a map of KEY: value or a list of "KEY=value" strings.
#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(untagged)]
enum Environment {
    Map(BTreeMap<String, String>),
    List(Vec<String>),
}

impl Default for Environment {
    fn default() -> Self {
        Environment::List(Vec::new())
    }
}

impl Environment {
    /// Normalizes into a list of "KEY=value" strings, suitable for `docker run -e`.
    fn to_pairs(&self) -> Vec<String> {
        match self {
            Environment::Map(m) => m.iter().map(|(k, v)| format!("{k}={v}")).collect(),
            Environment::List(l) => l.clone(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
struct ServiceConfig {
    name: String,
    internal_domain: String,
    #[serde(default)]
    build: Option<Build>,
    #[serde(default)]
    ports: Vec<String>,
    #[serde(default)]
    environment: Environment,
    #[serde(default)]
    dependencies: Vec<Dependency>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "kind")]
enum Dependency {
    #[serde(rename = "service")]
    Service {
        repo: String,
        default_branch: String,
    },
    #[serde(rename = "infra")]
    Infra {
        name: String,
        image: String,
        #[serde(default)]
        environment: Environment,
        #[serde(default)]
        ports: Vec<String>,
    },
    #[serde(rename = "shared-infra")]
    SharedInfra { service: String, name: String },
}

#[derive(Debug, Serialize, Clone)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub kind: String, // "service" | "infra" | "flow"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    pub downloaded: bool,
    /// Whether the on-disk checkout has uncommitted changes — see
    /// `concepts/branch-ownership-model.md`. Always `false` for stub
    /// (`downloaded: false`), infra, and flow nodes, which have no checkout.
    pub dirty: bool,
    /// names of the flows this node is reachable from, in the full resolved universe
    pub flows: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<NodeBuild>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub ports: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub environment: Vec<String>,
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
    pub kind: String, // "depends-on" | "owns" | "shared-infra"
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

/// Clones (or reuses) a bare mirror of `repo` under `workdir` so that
/// `git show <branch>:fghj.yaml` can read any branch's content without
/// needing a separate checkout per branch — used only by the review-run
/// branch-override path (`src/runs.rs`), which still needs to build an
/// arbitrary branch without touching the live workspace checkout.
pub fn ensure_mirror(repo: &str, workdir: &Path) -> Result<PathBuf> {
    let dir_name = repo
        .rsplit('/')
        .next()
        .unwrap_or(repo)
        .trim_end_matches(".git")
        .to_string();
    let mirror_path = workdir.join(format!("{dir_name}.git"));

    if !mirror_path.exists() {
        let status = Command::new("git")
            .args(["clone", "--quiet", "--mirror", repo])
            .arg(&mirror_path)
            .status()
            .with_context(|| format!("failed to run git clone --mirror for {repo}"))?;
        if !status.success() {
            bail!("git clone --mirror failed for {repo}");
        }
    }

    Ok(mirror_path)
}

/// Derives the conventional local checkout folder name from a git URL: the
/// last path segment, with a trailing `.git` stripped.
pub fn repo_name_from_url(repo: &str) -> String {
    repo.rsplit('/')
        .next()
        .unwrap_or(repo)
        .trim_end_matches(".git")
        .to_string()
}

/// Normalizes a git URL to a form that's stable across `git@host:org/repo.git`,
/// `https://host/org/repo.git` and `ssh://host/org/repo` spellings of the same
/// repo, so two dependents referencing the same repo don't get treated as
/// different repos just because they wrote the URL (or its `local_path`
/// override) differently.
fn normalize_repo_url(repo: &str) -> String {
    let mut u = repo.trim().to_string();
    for prefix in ["ssh://", "https://", "http://"] {
        if let Some(rest) = u.strip_prefix(prefix) {
            u = rest.to_string();
            break;
        }
    }
    if let Some(rest) = u.strip_prefix("git@") {
        u = rest.replacen(':', "/", 1);
    }
    u.trim_end_matches(".git").trim_end_matches('/').to_lowercase()
}

fn read_component_file(path: &Path) -> Result<ComponentConfig> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_yaml::from_str(&contents)
        .with_context(|| format!("failed to parse {} as a component config", path.display()))
}

/// Reads every immediate subdirectory of `workspace` that contains an
/// `fghj.yaml`, keyed by its local folder name (the convention-based repo id).
fn scan_workspace(workspace: &Path) -> Result<BTreeMap<String, ComponentConfig>> {
    let mut out = BTreeMap::new();
    if !workspace.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(workspace)
        .with_context(|| format!("failed to read workspace dir {}", workspace.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let config_path = path.join("fghj.yaml");
        if !config_path.exists() {
            continue;
        }
        let local_path = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let component = read_component_file(&config_path)?;
        out.insert(local_path, component);
    }
    Ok(out)
}

/// Reads the `origin` remote URL and checked-out branch of a real git working
/// tree, if any — used so a downloaded node still carries the `repo`/`branch`
/// info needed for the review-run branch-override path, even though
/// resolution itself no longer needs it to find the node on disk.
fn git_remote_and_branch(dir: &Path) -> (Option<String>, Option<String>) {
    let repo = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let branch = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    (repo, branch)
}

/// Whether a git working tree has uncommitted changes (or its status can't be
/// read at all — treated as dirty since we can't vouch for it being clean).
fn git_status_dirty(dir: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(true)
}

struct ResolveCtx<'a> {
    workspace: &'a Path,
    scanned: &'a BTreeMap<String, ComponentConfig>,
    /// normalized repo url -> local_path, for every repo actually on disk
    /// (keyed by its real git remote, not by whatever local_path a given
    /// dependent happens to guess/override).
    repo_index: HashMap<String, String>,
    /// normalized repo url -> stub node id, for repos not yet on disk. Ties
    /// together dependents that reference the same repo via different
    /// `local_path` spellings so they don't render as separate nodes.
    stub_repo_ids: HashMap<String, String>,
    nodes: HashMap<String, Node>,
    edges: Vec<Edge>,
    /// local paths already fully expanded, to avoid re-walking / infinite loops.
    visited: HashSet<String>,
    warnings: Vec<String>,
}

impl<'a> ResolveCtx<'a> {
    /// Registers (and, unless already visited, recursively expands) the service
    /// node for a repo already present on disk at `local_path`.
    fn visit_local_service(&mut self, local_path: &str, component: &ComponentConfig) -> String {
        let service_id = component.service.name.clone();

        self.nodes
            .entry(service_id.clone())
            .or_insert_with(|| {
                let dir = self.workspace.join(local_path);
                let (repo, branch) = git_remote_and_branch(&dir);
                let dirty = git_status_dirty(&dir);
                Node {
                id: service_id.clone(),
                label: service_id.clone(),
                kind: "service".into(),
                image: None,
                branch,
                repo,
                domain: Some(component.service.internal_domain.clone()),
                local_path: Some(local_path.to_string()),
                downloaded: true,
                dirty,
                flows: Vec::new(),
                build: component.service.build.as_ref().map(|b| NodeBuild {
                    context: b.context.clone(),
                    dockerfile: b.dockerfile.clone(),
                    args: b.args.clone(),
                }),
                ports: component.service.ports.clone(),
                environment: component.service.environment.to_pairs(),
                }
            });

        if !self.visited.insert(local_path.to_string()) {
            return service_id;
        }

        for dep in component.service.dependencies.clone() {
            self.visit_dependency(&service_id, dep);
        }

        service_id
    }

    /// Resolves a `Dependency::Service` reference to its conventional local
    /// path, then either recurses into it (if already on disk) or registers a
    /// `downloaded: false` stub node (if not) — never clones.
    fn visit_service_dependency(&mut self, owner_id: &str, repo: &str, default_branch: &str) -> String {
        let norm = normalize_repo_url(repo);

        // An on-disk match by the repo's real git remote wins over the plain
        // convention-derived name — matters if the repo was cloned by hand
        // (or via a different URL form) before fghj ever touched it.
        let local_path = self
            .repo_index
            .get(&norm)
            .cloned()
            .unwrap_or_else(|| repo_name_from_url(repo));

        let child_id = if let Some(component) = self.scanned.get(&local_path) {
            self.visit_local_service(&local_path, component)
        } else {
            // Not on disk yet: register a stub, deduped by repo url so the
            // same not-yet-pulled repo referenced with different local_path
            // spellings still renders as a single node. The stub's id is
            // whichever local_path was seen first (its real service name is
            // unknown until pulled).
            let stub_id = self
                .stub_repo_ids
                .entry(norm.clone())
                .or_insert_with(|| local_path.clone())
                .clone();
            self.nodes.entry(stub_id.clone()).or_insert_with(|| Node {
                id: stub_id.clone(),
                label: stub_id.clone(),
                kind: "service".into(),
                image: None,
                branch: Some(default_branch.to_string()),
                repo: Some(repo.to_string()),
                domain: None,
                local_path: Some(stub_id.clone()),
                downloaded: false,
                dirty: false,
                flows: Vec::new(),
                build: None,
                ports: Vec::new(),
                environment: Vec::new(),
            });
            stub_id
        };

        self.edges.push(Edge {
            from: owner_id.to_string(),
            to: child_id.clone(),
            kind: "depends-on".into(),
            branch: Some(default_branch.to_string()),
            flows: Vec::new(),
        });

        child_id
    }

    fn visit_dependency(&mut self, owner_id: &str, dep: Dependency) {
        match dep {
            Dependency::Service {
                repo,
                default_branch,
            } => {
                self.visit_service_dependency(owner_id, &repo, &default_branch);
            }
            Dependency::Infra {
                name,
                image,
                environment,
                ports,
            } => {
                let infra_id = format!("{owner_id}/{name}");
                self.nodes.entry(infra_id.clone()).or_insert_with(|| Node {
                    id: infra_id.clone(),
                    label: name.clone(),
                    kind: "infra".into(),
                    image: Some(image),
                    branch: None,
                    repo: None,
                    domain: None,
                    local_path: None,
                    downloaded: true,
                    dirty: false,
                    flows: Vec::new(),
                    build: None,
                    ports,
                    environment: environment.to_pairs(),
                });
                self.edges.push(Edge {
                    from: owner_id.to_string(),
                    to: infra_id,
                    kind: "owns".into(),
                    branch: None,
                    flows: Vec::new(),
                });
            }
            Dependency::SharedInfra { service, name } => {
                let infra_id = format!("{service}/{name}");
                self.edges.push(Edge {
                    from: owner_id.to_string(),
                    to: infra_id,
                    kind: "shared-infra".into(),
                    branch: None,
                    flows: Vec::new(),
                });
            }
        }
    }
}

/// Resolves every flow declared by every repo currently on disk in the
/// workspace into one shared graph — the "static full universe" the UI lays
/// out once. Any repo can declare `flows`; there is no distinguished "root"
/// repo. A dependency whose local folder isn't present in the workspace is
/// rendered as a `downloaded: false` stub instead of blocking resolution —
/// call `pull_all` to clone everything reachable and try again.
pub fn resolve_universe(workspace: &Path) -> Result<Graph> {
    let scanned = scan_workspace(workspace)?;

    let mut repo_index: HashMap<String, String> = HashMap::new();
    for local_path in scanned.keys() {
        let (repo, _branch) = git_remote_and_branch(&workspace.join(local_path));
        if let Some(repo) = repo {
            repo_index
                .entry(normalize_repo_url(&repo))
                .or_insert_with(|| local_path.clone());
        }
    }

    let mut ctx = ResolveCtx {
        workspace,
        scanned: &scanned,
        repo_index,
        stub_repo_ids: HashMap::new(),
        nodes: HashMap::new(),
        edges: Vec::new(),
        visited: HashSet::new(),
        warnings: Vec::new(),
    };

    // (flow_name, owner_service_id) — the flow's own repo is the BFS root for
    // reachability tagging below. There is no separate "flow" node in the
    // graph: a flow is a named lens over the repo graph, not a repo itself.
    let mut flow_roots: Vec<(String, String)> = Vec::new();
    for (local_path, component) in &scanned {
        for (flow_name, flow) in &component.flows {
            let owner_id = ctx.visit_local_service(local_path, component);
            flow_roots.push((flow_name.to_string(), owner_id.clone()));

            for dep in flow.dependencies.clone() {
                ctx.visit_dependency(&owner_id, dep);
            }
        }
    }

    // Every repo actually on disk is part of the map, whether or not any flow
    // reaches it — flows are a highlight overlay, not a visibility filter.
    // This is what makes an entry repo with no flows (or one nothing else
    // references) still show up, along with its own infra/service deps.
    for (local_path, component) in &scanned {
        ctx.visit_local_service(local_path, component);
    }

    // Validate shared-infra references resolve to a real, resolved infra node.
    let known_ids: HashSet<&str> = ctx.nodes.keys().map(|s| s.as_str()).collect();
    for edge in &ctx.edges {
        if edge.kind == "shared-infra" && !known_ids.contains(edge.to.as_str()) {
            ctx.warnings.push(format!(
                "dangling shared-infra reference: '{}' points at '{}', which was never resolved as an infra dependency",
                edge.from, edge.to
            ));
        }
    }

    // Per-flow reachability: BFS from each flow's root over depends-on/owns edges
    // (shared-infra is a cross-reference, not a structural membership edge).
    let mut adjacency: HashMap<&str, Vec<(usize, &str)>> = HashMap::new();
    for (idx, edge) in ctx.edges.iter().enumerate() {
        if edge.kind == "depends-on" || edge.kind == "owns" {
            adjacency
                .entry(edge.from.as_str())
                .or_default()
                .push((idx, edge.to.as_str()));
        }
    }

    let mut node_flows: HashMap<String, Vec<String>> = HashMap::new();
    let mut edge_flows: Vec<HashSet<String>> = vec![HashSet::new(); ctx.edges.len()];

    for (flow_name, root_id) in &flow_roots {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut queue = vec![root_id.as_str()];
        seen.insert(root_id.as_str());
        node_flows
            .entry(root_id.clone())
            .or_default()
            .push(flow_name.clone());

        while let Some(id) = queue.pop() {
            for &(edge_idx, next) in adjacency.get(id).unwrap_or(&Vec::new()) {
                edge_flows[edge_idx].insert(flow_name.clone());
                if seen.insert(next) {
                    node_flows
                        .entry(next.to_string())
                        .or_default()
                        .push(flow_name.clone());
                    queue.push(next);
                }
            }
        }
    }

    // shared-infra edges are cross-references, not structural edges, so they were
    // excluded from the BFS above — inherit flow membership from their `from` node instead.
    for (edge, flows) in ctx.edges.iter().zip(edge_flows.iter_mut()) {
        if edge.kind == "shared-infra" {
            if let Some(fl) = node_flows.get(&edge.from) {
                flows.extend(fl.iter().cloned());
            }
        }
    }

    // Sorted by id so node order — and therefore the frontend's computed
    // layout — is stable across polls. `ctx.nodes` is a HashMap, so without
    // this, each `resolve_universe` call could hand back the same nodes in a
    // different order and make the graph visibly jump on every 3s refresh,
    // for reasons unrelated to which flow is selected.
    let mut nodes: Vec<Node> = ctx.nodes.into_values().collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    for node in &mut nodes {
        if let Some(fl) = node_flows.remove(&node.id) {
            node.flows = fl;
        }
    }

    let mut edges = ctx.edges;
    for (edge, flows) in edges.iter_mut().zip(edge_flows.into_iter()) {
        let mut fl: Vec<String> = flows.into_iter().collect();
        fl.sort();
        edge.flows = fl;
    }

    let workspace_name = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();

    Ok(Graph {
        workspace_name,
        nodes,
        edges,
        warnings: ctx.warnings,
    })
}

// Pulling (`git clone`)-ing missing nodes now happens through
// `downloads::DownloadRegistry`, which runs clones in a background thread and
// streams their output to the UI instead of blocking the request. See
// `src/downloads.rs`.
