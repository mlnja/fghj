use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Clone)]
struct FlowConfig {
    #[allow(dead_code)]
    description: Option<String>,
    #[serde(default)]
    service: Option<String>,
    dependencies: Vec<Dependency>,
}

#[derive(Debug, Deserialize, Clone)]
struct ComponentConfig {
    #[allow(dead_code)]
    version: String,
    services: BTreeMap<String, ServiceConfig>,
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

fn default_domain_scope() -> String {
    "run".to_string()
}

fn default_restart() -> String {
    "no".to_string()
}

/// Mirrors `#Healthcheck` in `schema/dependency.cue`. `interval`/`timeout`/
/// `start_period` are in seconds here — converted to the nanoseconds
/// Docker's API wants at the `docker::run_container` boundary.
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct Healthcheck {
    pub test: Vec<String>,
    #[serde(default)]
    pub interval: Option<u64>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub start_period: Option<u64>,
    #[serde(default)]
    pub retries: Option<u64>,
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
    #[serde(default)]
    build: Option<Build>,
    #[serde(default)]
    ports: BTreeMap<String, PortConfig>,
    #[serde(default = "default_domain_scope")]
    domain_scope: String,
    #[serde(default)]
    environment: Environment,
    #[serde(default)]
    env_file: Vec<String>,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    volumes: Vec<VolumeMount>,
    #[serde(default)]
    additional_hosts: Vec<String>,
    #[serde(default = "default_restart")]
    restart: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    cap_add: Vec<String>,
    #[serde(default)]
    cap_drop: Vec<String>,
    #[serde(default)]
    privileged: bool,
    #[serde(default)]
    extra_hosts: Vec<String>,
    #[serde(default)]
    healthcheck: Option<Healthcheck>,
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    dependencies: Vec<Dependency>,
}

/// A bind mount or a named volume, on either a service or a backing
/// dependency. Mirrors `#Volume` in `schema/component.cue` — the untagged
/// shapes match its `{host,...}` vs `{name,scope,...}` disjunction directly.
/// A named volume's real Docker name is derived (never author-declared),
/// the same way a node's domain is — see `runs::derive_volume_name` — so
/// two nodes anywhere in the graph declaring the same `name` + `scope`
/// transparently share the same underlying storage.
#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(untagged)]
pub enum VolumeMount {
    Bind {
        host: String,
        container: String,
        #[serde(default)]
        read_only: bool,
    },
    Named {
        name: String,
        #[serde(default = "default_domain_scope")]
        scope: String,
        container: String,
        #[serde(default)]
        read_only: bool,
    },
}

/// A declared container port and its role. `primary` (at most one per node)
/// puts it at the node's own derived domain; `name` gives it an additional
/// nested domain `{name}.{node's domain}` — `runs::start_node` derives both
/// the same way it derives the node's own domain, so a named port can never
/// collide across runs/workspaces either. Neither set: still published to an
/// ephemeral localhost port, just with no `*.fghj.internal` name. Mirrors
/// `#Port` in `schema/component.cue`.
#[derive(Debug, Default, Deserialize, Clone, Serialize)]
pub struct PortConfig {
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub host_port: Option<u16>,
}

/// The fields of a `Dependency::Backing` — pulled out into its own struct
/// (behind a `Box` at the use site) rather than inlined as a large struct
/// variant, since `Dependency::Service`/`SharedBacking` are tiny by
/// comparison and clippy's `large_enum_variant` flags the size gap
/// otherwise.
#[derive(Debug, Deserialize, Clone)]
struct BackingDependencyConfig {
    name: String,
    image: String,
    #[serde(default)]
    environment: Environment,
    #[serde(default)]
    ports: Vec<String>,
    #[serde(default = "default_domain_scope")]
    domain_scope: String,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    volumes: Vec<VolumeMount>,
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    env_file: Vec<String>,
    #[serde(default = "default_restart")]
    restart: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    cap_add: Vec<String>,
    #[serde(default)]
    cap_drop: Vec<String>,
    #[serde(default)]
    privileged: bool,
    #[serde(default)]
    extra_hosts: Vec<String>,
    #[serde(default)]
    healthcheck: Option<Healthcheck>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "kind")]
enum Dependency {
    #[serde(rename = "service")]
    Service {
        #[serde(default)]
        repo: Option<String>,
        #[serde(default)]
        default_branch: Option<String>,
        #[serde(default)]
        services: Vec<String>,
    },
    #[serde(rename = "backing")]
    Backing(Box<BackingDependencyConfig>),
    #[serde(rename = "shared-backing")]
    SharedBacking {
        #[serde(default)]
        repo: Option<String>,
        service: String,
        name: String,
    },
}

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
    /// repo is actually pulled and its `fghj.yaml` read.
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
    /// only — see `schema/component.cue`'s `#AdditionalHost`), routed to its
    /// `primary` port by `runs::start_node`. Always empty for backing and
    /// stub nodes.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub additional_hosts: Vec<String>,
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

/// Clones (or reuses) a bare mirror of `repo` under `workdir` so that
/// `git show <branch>:fghj.yaml` can read any branch's content without
/// needing a separate checkout per branch — used only by the review-run
/// branch-override path (`src/runs.rs`), which still needs to build an
/// arbitrary branch without touching the live workspace checkout.
/// `owner` drops the clone's privileges back to the real user who wired the
/// workspace (see [`crate::store::WorkspaceOwner`]) — `fghjd` runs as root
/// and has no SSH credentials of its own for a private remote. Pass `None`
/// when already running as the correct user.
pub fn ensure_mirror(
    repo: &str,
    workdir: &Path,
    owner: Option<&crate::store::WorkspaceOwner>,
) -> Result<PathBuf> {
    let dir_name = repo
        .rsplit('/')
        .next()
        .unwrap_or(repo)
        .trim_end_matches(".git")
        .to_string();
    let mirror_path = workdir.join(format!("{dir_name}.git"));

    if !mirror_path.exists() {
        let mut cmd = Command::new("git");
        cmd.args(["clone", "--quiet", "--mirror", repo])
            .arg(&mirror_path);
        if let Some(owner) = owner {
            owner.apply_to_command(&mut cmd);
        }
        crate::store::harden_git_ssh(&mut cmd);
        let output = cmd
            .output()
            .with_context(|| format!("failed to run git clone --mirror for {repo}"))?;
        if !output.status.success() {
            bail!(
                "git clone --mirror failed for {repo}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
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
    u.trim_end_matches(".git")
        .trim_end_matches('/')
        .to_lowercase()
}

fn read_component_file(path: &Path) -> Result<ComponentConfig> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
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
    /// Registers (and, unless already visited, recursively expands) every
    /// service node declared by a repo already present on disk at
    /// `local_path`. Returns the id of each, keyed by its name in
    /// `component.services` — cheap to call again for an already-visited
    /// repo (just returns the same ids without re-walking dependencies).
    fn visit_local_services(
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
                    additional_hosts: service.additional_hosts.clone(),
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
    fn visit_local_service(
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

    /// Warns (non-fatally) when a service declares more than one `primary`
    /// port — at most one port can sit at the service's own derived domain.
    /// Everything `check_http_routes` used to check (a route naming a port
    /// the service never declared) is now structurally impossible: port and
    /// role are one `ports` map entry, not two lists to keep in sync.
    fn check_ports(&mut self, service_id: &str, service: &ServiceConfig) {
        let primaries: Vec<&str> = service
            .ports
            .iter()
            .filter(|(_, cfg)| cfg.primary)
            .map(|(port, _)| port.as_str())
            .collect();
        if primaries.len() > 1 {
            self.warnings.push(format!(
                "'{service_id}' declares more than one primary port ({}); only one can sit at its own domain",
                primaries.join(", ")
            ));
        }
        if !service.additional_hosts.is_empty() && primaries.is_empty() {
            self.warnings.push(format!(
                "'{service_id}' declares additional_hosts but no primary port; those hosts won't be routed to anything"
            ));
        }
    }

    /// Resolves a `Dependency::Service` reference to its conventional local
    /// path, then either recurses into it (if already on disk) or registers a
    /// `downloaded: false` stub node (if not) — never clones. `wanted_service`
    /// picks which of the target repo's declared services this depends on
    /// (see `visit_local_service`); returns `None` (after pushing a warning)
    /// if that's ambiguous or unresolvable, without creating a `depends-on`
    /// edge at all.
    fn visit_service_dependency(
        &mut self,
        owner_id: &str,
        repo: &str,
        default_branch: &str,
        wanted_service: Option<&str>,
    ) -> Option<String> {
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
            self.visit_local_service(&local_path, component, wanted_service)?
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
                domain_scope: default_domain_scope(),
                local_path: Some(stub_id.clone()),
                domain: String::new(),
                downloaded: false,
                dirty: false,
                flows: Vec::new(),
                build: None,
                ports: BTreeMap::new(),
                environment: Vec::new(),
                command: Vec::new(),
                volumes: Vec::new(),
                additional_hosts: Vec::new(),
                env_file: Vec::new(),
                restart: default_restart(),
                user: None,
                working_dir: None,
                labels: BTreeMap::new(),
                cap_add: Vec::new(),
                cap_drop: Vec::new(),
                privileged: false,
                extra_hosts: Vec::new(),
                healthcheck: None,
                platform: None,
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

        Some(child_id)
    }

    /// Same-repo counterpart to `visit_service_dependency`: a `kind: service`
    /// dependency that omitted `repo` names a sibling service already
    /// declared in this same repo's own `services:` map — nothing to clone,
    /// no stub, just a `depends-on` edge (or a warning, same as
    /// `visit_local_service`, if `wanted_service` is ambiguous or missing).
    fn visit_sibling_service_dependency(
        &mut self,
        owner_id: &str,
        local_path: &str,
        wanted_service: Option<&str>,
    ) -> Option<String> {
        let component = self.scanned.get(local_path)?;
        let child_id = self.visit_local_service(local_path, component, wanted_service)?;
        if child_id == owner_id {
            self.warnings.push(format!(
                "'{owner_id}' declares a same-repo `kind: service` dependency on itself"
            ));
            return None;
        }
        self.edges.push(Edge {
            from: owner_id.to_string(),
            to: child_id.clone(),
            kind: "depends-on".into(),
            branch: None,
            flows: Vec::new(),
        });
        Some(child_id)
    }

    fn visit_dependency(&mut self, owner_id: &str, local_path: &str, dep: Dependency) {
        match dep {
            Dependency::Service {
                repo,
                default_branch,
                services,
            } => {
                // One dependency block can name several of the target repo's
                // services (see `#GitDependency.services`) — resolving to a
                // `local_path` (or, for the same-repo form below, reusing the
                // owner's own) happens once inside each call, and
                // `visit_local_services` is itself idempotent per repo (see
                // its `self.visited` guard), so repeating this per name costs
                // nothing extra beyond the one `depends-on` edge each needs.
                // No names given at all: same as before, depend on "the"
                // service (the sole one, or a warning if that's ambiguous).
                let wanted: Vec<Option<&str>> = if services.is_empty() {
                    vec![None]
                } else {
                    services.iter().map(|n| Some(n.as_str())).collect()
                };
                match repo {
                    Some(repo) => {
                        let default_branch = default_branch.unwrap_or_default();
                        for name in wanted {
                            self.visit_service_dependency(owner_id, &repo, &default_branch, name);
                        }
                    }
                    None => {
                        // Same repo, no `repo:` given — reference a sibling
                        // service already declared in this repo's own
                        // `services:` map instead of cloning anything.
                        for name in wanted {
                            self.visit_sibling_service_dependency(owner_id, local_path, name);
                        }
                    }
                }
            }
            Dependency::Backing(backing) => {
                let BackingDependencyConfig {
                    name,
                    image,
                    environment,
                    ports,
                    domain_scope,
                    command,
                    volumes,
                    platform,
                    env_file,
                    restart,
                    user,
                    working_dir,
                    labels,
                    cap_add,
                    cap_drop,
                    privileged,
                    extra_hosts,
                    healthcheck,
                } = *backing;
                // Leaf-first, same convention as service ids and named
                // ports (`{port_name}.{node's domain}`): the specific thing
                // comes first, its owning scope after.
                let backing_id = format!("{name}.{owner_id}");
                self.nodes
                    .entry(backing_id.clone())
                    .or_insert_with(|| Node {
                        id: backing_id.clone(),
                        label: name.clone(),
                        kind: "backing".into(),
                        image: Some(image),
                        branch: None,
                        repo: None,
                        domain_scope,
                        local_path: None,
                        domain: String::new(),
                        downloaded: true,
                        dirty: false,
                        flows: Vec::new(),
                        build: None,
                        ports: ports
                            .into_iter()
                            .map(|p| (p, PortConfig::default()))
                            .collect(),
                        environment: environment.to_pairs(),
                        command,
                        volumes,
                        additional_hosts: Vec::new(),
                        env_file,
                        restart,
                        user,
                        working_dir,
                        labels,
                        cap_add,
                        cap_drop,
                        privileged,
                        extra_hosts,
                        healthcheck,
                        platform,
                    });
                self.edges.push(Edge {
                    from: owner_id.to_string(),
                    to: backing_id,
                    kind: "owns".into(),
                    branch: None,
                    flows: Vec::new(),
                });
            }
            Dependency::SharedBacking {
                repo,
                service,
                name,
            } => {
                // References the owning service by `repo` (if given, like
                // `#GitDependency`) + `service` — a bare service name alone
                // isn't unique (two peer repos, or two services in the same
                // repo, can share one), so both are needed to identify one
                // unambiguous node. Omitting `repo` means "a sibling service
                // in this same repo" (`local_path`, passed in by the caller).
                // Resolved without registering a `depends-on` edge or
                // recursing into it (this is a reference to an
                // already-declared backing dependency, not a new one).
                let target_local_path = match &repo {
                    Some(repo) => {
                        let norm = normalize_repo_url(repo);
                        self.repo_index
                            .get(&norm)
                            .cloned()
                            .unwrap_or_else(|| repo_name_from_url(repo))
                    }
                    None => local_path.to_string(),
                };
                let target_owner_id = format!("{service}.{target_local_path}");
                let backing_id = format!("{name}.{target_owner_id}");
                self.edges.push(Edge {
                    from: owner_id.to_string(),
                    to: backing_id,
                    kind: "shared-backing".into(),
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
            let Some(owner_id) =
                ctx.visit_local_service(local_path, component, flow.service.as_deref())
            else {
                continue;
            };
            flow_roots.push((flow_name.to_string(), owner_id.clone()));

            for dep in flow.dependencies.clone() {
                ctx.visit_dependency(&owner_id, local_path, dep);
            }
        }
    }

    // Every repo actually on disk is part of the map, whether or not any flow
    // reaches it — flows are a highlight overlay, not a visibility filter.
    // This is what makes an entry repo with no flows (or one nothing else
    // references) still show up, along with its own backing/service deps.
    for (local_path, component) in &scanned {
        ctx.visit_local_services(local_path, component);
    }

    // Validate shared-backing references resolve to a real, resolved backing node.
    let known_ids: HashSet<&str> = ctx.nodes.keys().map(|s| s.as_str()).collect();
    for edge in &ctx.edges {
        if edge.kind == "shared-backing" && !known_ids.contains(edge.to.as_str()) {
            ctx.warnings.push(format!(
                "dangling shared-backing reference: '{}' points at '{}', which was never resolved as a backing dependency",
                edge.from, edge.to
            ));
        }
    }

    // Per-flow reachability: BFS from each flow's root over depends-on/owns edges
    // (shared-backing is a cross-reference, not a structural membership edge).
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

    // shared-backing edges are cross-references, not structural edges, so they were
    // excluded from the BFS above — inherit flow membership from their `from` node instead.
    for (edge, flows) in ctx.edges.iter().zip(edge_flows.iter_mut()) {
        if edge.kind == "shared-backing"
            && let Some(fl) = node_flows.get(&edge.from)
        {
            flows.extend(fl.iter().cloned());
        }
    }

    let workspace_name = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();

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
        // Default-run domain, always known once a node's id and
        // domain_scope are — see the field's own doc comment for why this
        // isn't just set at construction time.
        node.domain = crate::runs::derive_domain(
            &node.id,
            &node.domain_scope,
            &workspace_name,
            crate::runs::DEFAULT_RUN_ID,
        );
    }

    let mut edges = ctx.edges;
    for (edge, flows) in edges.iter_mut().zip(edge_flows.into_iter()) {
        let mut fl: Vec<String> = flows.into_iter().collect();
        fl.sort();
        edge.flows = fl;
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a component config with a single service named after
    /// `local_path` (matching every test's own convention) — `service_yaml`
    /// is the body that used to sit directly under a singular `service:`
    /// field (no `name:` line; the map key is the name now), reindented one
    /// level deeper to sit under `services:\n  {local_path}:`.
    fn write_component(workspace: &Path, local_path: &str, service_yaml: &str) {
        let dir = workspace.join(local_path);
        fs::create_dir_all(&dir).unwrap();
        let indented: String = service_yaml
            .lines()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let body = if indented.trim().is_empty() {
            format!("  {local_path}: {{}}\n")
        } else {
            format!("  {local_path}:\n{indented}\n")
        };
        fs::write(
            dir.join("fghj.yaml"),
            format!("version: \"1.0\"\nservices:\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn service_domain_scope_defaults_to_run_and_can_opt_into_stable() {
        let tmp = tempfile::tempdir().unwrap();
        write_component(tmp.path(), "svc-a", "");
        write_component(tmp.path(), "svc-b", "  domain_scope: stable\n");

        let graph = resolve_universe(tmp.path()).unwrap();

        let a = graph.nodes.iter().find(|n| n.id == "svc-a.svc-a").unwrap();
        let b = graph.nodes.iter().find(|n| n.id == "svc-b.svc-b").unwrap();
        assert_eq!(a.domain_scope, "run");
        assert_eq!(b.domain_scope, "stable");
    }

    #[test]
    fn ports_round_trip_into_graph_nodes() {
        let tmp = tempfile::tempdir().unwrap();
        write_component(
            tmp.path(),
            "myservice",
            "  ports:\n\
             \x20   \"8080\":\n\
             \x20     primary: true\n\
             \x20   \"9090\":\n\
             \x20     name: metrics\n",
        );

        let graph = resolve_universe(tmp.path()).unwrap();

        let node = graph
            .nodes
            .iter()
            .find(|n| n.id == "myservice.myservice")
            .unwrap();
        assert_eq!(node.ports.len(), 2);
        assert!(node.ports["8080"].primary);
        assert_eq!(node.ports["9090"].name.as_deref(), Some("metrics"));
        assert!(graph.warnings.is_empty());
    }

    #[test]
    fn warns_when_more_than_one_port_is_primary() {
        let tmp = tempfile::tempdir().unwrap();
        write_component(
            tmp.path(),
            "myservice",
            "  ports:\n\
             \x20   \"8080\":\n\
             \x20     primary: true\n\
             \x20   \"9090\":\n\
             \x20     primary: true\n",
        );

        let graph = resolve_universe(tmp.path()).unwrap();

        assert!(
            graph
                .warnings
                .iter()
                .any(|w| w.contains("myservice") && w.contains("8080") && w.contains("9090"))
        );
    }

    #[test]
    fn service_bind_mount_round_trips_into_graph_node() {
        let tmp = tempfile::tempdir().unwrap();
        write_component(
            tmp.path(),
            "myservice",
            "  volumes:\n\
             \x20   - host: ./src\n\
             \x20     container: /app/src\n\
             \x20   - host: ../intel\n\
             \x20     container: /app/intel\n\
             \x20     read_only: true\n",
        );

        let graph = resolve_universe(tmp.path()).unwrap();

        let node = graph
            .nodes
            .iter()
            .find(|n| n.id == "myservice.myservice")
            .unwrap();
        assert_eq!(node.volumes.len(), 2);
        assert!(matches!(
            &node.volumes[0],
            VolumeMount::Bind { host, container, read_only }
                if host == "./src" && container == "/app/src" && !read_only
        ));
        assert!(matches!(
            &node.volumes[1],
            VolumeMount::Bind { host, container, read_only }
                if host == "../intel" && container == "/app/intel" && *read_only
        ));
    }

    #[test]
    fn named_volume_round_trips_into_graph_node() {
        let tmp = tempfile::tempdir().unwrap();
        write_component(
            tmp.path(),
            "myservice",
            "  dependencies:\n\
             \x20   - kind: backing\n\
             \x20     name: postgres\n\
             \x20     image: postgres:16\n\
             \x20     ports: [\"5432\"]\n\
             \x20     volumes:\n\
             \x20       - name: pgdata\n\
             \x20         container: /var/lib/postgresql/data\n",
        );

        let graph = resolve_universe(tmp.path()).unwrap();

        let node = graph
            .nodes
            .iter()
            .find(|n| n.id == "postgres.myservice.myservice")
            .unwrap();
        assert_eq!(node.volumes.len(), 1);
        assert!(matches!(
            &node.volumes[0],
            VolumeMount::Named { name, scope, container, read_only }
                if name == "pgdata" && scope == "run" && container == "/var/lib/postgresql/data" && !read_only
        ));
    }

    #[test]
    fn command_round_trips_into_graph_node_for_service_and_backing() {
        let tmp = tempfile::tempdir().unwrap();
        write_component(
            tmp.path(),
            "myservice",
            "  command: [\"npm\", \"run\", \"dev\"]\n\
             \x20 dependencies:\n\
             \x20   - kind: backing\n\
             \x20     name: mysql\n\
             \x20     image: mysql:8.0.33\n\
             \x20     ports: [\"3306\"]\n\
             \x20     command: [\"mysqld\", \"--sql_mode=NO_ENGINE_SUBSTITUTION\"]\n",
        );

        let graph = resolve_universe(tmp.path()).unwrap();

        let service = graph
            .nodes
            .iter()
            .find(|n| n.id == "myservice.myservice")
            .unwrap();
        assert_eq!(service.command, vec!["npm", "run", "dev"]);

        let backing = graph
            .nodes
            .iter()
            .find(|n| n.id == "mysql.myservice.myservice")
            .unwrap();
        assert_eq!(
            backing.command,
            vec!["mysqld", "--sql_mode=NO_ENGINE_SUBSTITUTION"]
        );
    }

    #[test]
    fn run_options_round_trip_into_graph_node_for_service_and_backing() {
        let tmp = tempfile::tempdir().unwrap();
        write_component(
            tmp.path(),
            "myservice",
            "  env_file:\n\
             \x20   - .env\n\
             \x20 platform: linux/arm64\n\
             \x20 restart: always\n\
             \x20 user: \"1000:1000\"\n\
             \x20 working_dir: /app\n\
             \x20 labels:\n\
             \x20   team: platform\n\
             \x20 cap_add: [\"NET_ADMIN\"]\n\
             \x20 cap_drop: [\"ALL\"]\n\
             \x20 privileged: true\n\
             \x20 extra_hosts:\n\
             \x20   - \"metadata:169.254.169.254\"\n\
             \x20 dependencies:\n\
             \x20   - kind: backing\n\
             \x20     name: postgres\n\
             \x20     image: postgres:16\n\
             \x20     ports: [\"5432\"]\n\
             \x20     platform: linux/amd64\n\
             \x20     env_file:\n\
             \x20       - .env.postgres\n\
             \x20     restart: unless-stopped\n\
             \x20     healthcheck:\n\
             \x20       test: [\"CMD\", \"pg_isready\"]\n\
             \x20       interval: 5\n\
             \x20       retries: 3\n",
        );

        let graph = resolve_universe(tmp.path()).unwrap();

        let service = graph
            .nodes
            .iter()
            .find(|n| n.id == "myservice.myservice")
            .unwrap();
        assert_eq!(service.env_file, vec![".env"]);
        assert_eq!(service.restart, "always");
        assert_eq!(service.user.as_deref(), Some("1000:1000"));
        assert_eq!(service.working_dir.as_deref(), Some("/app"));
        assert_eq!(
            service.labels.get("team").map(String::as_str),
            Some("platform")
        );
        assert_eq!(service.cap_add, vec!["NET_ADMIN"]);
        assert_eq!(service.cap_drop, vec!["ALL"]);
        assert!(service.privileged);
        assert_eq!(service.extra_hosts, vec!["metadata:169.254.169.254"]);
        assert_eq!(service.platform.as_deref(), Some("linux/arm64"));

        let backing = graph
            .nodes
            .iter()
            .find(|n| n.id == "postgres.myservice.myservice")
            .unwrap();
        assert_eq!(backing.platform.as_deref(), Some("linux/amd64"));
        assert_eq!(backing.env_file, vec![".env.postgres"]);
        assert_eq!(backing.restart, "unless-stopped");
        let hc = backing.healthcheck.as_ref().unwrap();
        assert_eq!(hc.test, vec!["CMD", "pg_isready"]);
        assert_eq!(hc.interval, Some(5));
        assert_eq!(hc.retries, Some(3));
    }

    #[test]
    fn run_options_default_to_compose_equivalent_no_ops() {
        let tmp = tempfile::tempdir().unwrap();
        write_component(tmp.path(), "myservice", "");

        let graph = resolve_universe(tmp.path()).unwrap();
        let service = graph
            .nodes
            .iter()
            .find(|n| n.id == "myservice.myservice")
            .unwrap();
        assert_eq!(service.restart, "no");
        assert!(service.user.is_none());
        assert!(service.labels.is_empty());
        assert!(!service.privileged);
        assert!(service.healthcheck.is_none());
    }

    #[test]
    fn additional_hosts_round_trip_into_graph_node() {
        let tmp = tempfile::tempdir().unwrap();
        write_component(
            tmp.path(),
            "myservice",
            "  ports:\n\
             \x20   \"8080\":\n\
             \x20     primary: true\n\
             \x20 additional_hosts:\n\
             \x20   - aikido.local\n\
             \x20   - demo.example.com\n",
        );

        let graph = resolve_universe(tmp.path()).unwrap();

        let node = graph
            .nodes
            .iter()
            .find(|n| n.id == "myservice.myservice")
            .unwrap();
        assert_eq!(
            node.additional_hosts,
            vec!["aikido.local", "demo.example.com"]
        );
        assert!(graph.warnings.is_empty());
    }

    #[test]
    fn warns_when_additional_hosts_declared_without_a_primary_port() {
        let tmp = tempfile::tempdir().unwrap();
        write_component(
            tmp.path(),
            "myservice",
            "  additional_hosts:\n\
             \x20   - aikido.local\n",
        );

        let graph = resolve_universe(tmp.path()).unwrap();

        assert!(
            graph
                .warnings
                .iter()
                .any(|w| w.contains("myservice") && w.contains("additional_hosts"))
        );
    }

    /// `kind: service`'s `services:` list lets one dependency block (one
    /// `repo`/`default_branch`) name several services owned by the same
    /// target repo — this is what replaces repeating a whole block per
    /// service. Depends on `repo-b` declaring two services (`api`, `jobs`)
    /// and asserts both get their own node and their own `depends-on` edge
    /// from the single dependency block in `repo-a`.
    #[test]
    fn git_dependency_services_list_targets_multiple_services_in_one_repo() {
        let tmp = tempfile::tempdir().unwrap();

        fs::create_dir_all(tmp.path().join("repo-b")).unwrap();
        fs::write(
            tmp.path().join("repo-b/fghj.yaml"),
            "version: \"1.0\"\n\
             services:\n\
             \x20 api:\n\
             \x20   build:\n\
             \x20     context: .\n\
             \x20 jobs:\n\
             \x20   build:\n\
             \x20     context: .\n",
        )
        .unwrap();

        fs::create_dir_all(tmp.path().join("repo-a")).unwrap();
        fs::write(
            tmp.path().join("repo-a/fghj.yaml"),
            "version: \"1.0\"\n\
             services:\n\
             \x20 web:\n\
             \x20   build:\n\
             \x20     context: .\n\
             \x20   dependencies:\n\
             \x20     - kind: service\n\
             \x20       repo: https://example.com/repo-b.git\n\
             \x20       default_branch: main\n\
             \x20       services: [api, jobs]\n",
        )
        .unwrap();

        let graph = resolve_universe(tmp.path()).unwrap();

        assert!(graph.nodes.iter().any(|n| n.id == "api.repo-b"));
        assert!(graph.nodes.iter().any(|n| n.id == "jobs.repo-b"));

        let depends_on: Vec<&str> = graph
            .edges
            .iter()
            .filter(|e| e.from == "web.repo-a" && e.kind == "depends-on")
            .map(|e| e.to.as_str())
            .collect();
        assert_eq!(depends_on.len(), 2);
        assert!(depends_on.contains(&"api.repo-b"));
        assert!(depends_on.contains(&"jobs.repo-b"));
        assert!(graph.warnings.is_empty());
    }

    /// Omitting `services:` entirely on a `kind: service` dependency still
    /// defaults to "the" service when the target repo declares exactly one
    /// — unchanged behavior, kept as a regression check against the new
    /// list-shaped field.
    #[test]
    fn git_dependency_without_services_defaults_to_the_sole_service() {
        let tmp = tempfile::tempdir().unwrap();
        write_component(tmp.path(), "repo-b", "");

        fs::create_dir_all(tmp.path().join("repo-a")).unwrap();
        fs::write(
            tmp.path().join("repo-a/fghj.yaml"),
            "version: \"1.0\"\n\
             services:\n\
             \x20 web:\n\
             \x20   build:\n\
             \x20     context: .\n\
             \x20   dependencies:\n\
             \x20     - kind: service\n\
             \x20       repo: https://example.com/repo-b.git\n\
             \x20       default_branch: main\n",
        )
        .unwrap();

        let graph = resolve_universe(tmp.path()).unwrap();

        assert!(
            graph
                .edges
                .iter()
                .any(|e| e.from == "web.repo-a" && e.to == "repo-b.repo-b")
        );
        assert!(graph.warnings.is_empty());
    }

    #[test]
    fn git_dependency_without_repo_targets_a_sibling_service_in_the_same_repo() {
        let tmp = tempfile::tempdir().unwrap();

        fs::create_dir_all(tmp.path().join("shop-web")).unwrap();
        fs::write(
            tmp.path().join("shop-web/fghj.yaml"),
            "version: \"1.0\"\n\
             services:\n\
             \x20 vite:\n\
             \x20   build:\n\
             \x20     context: .\n\
             \x20   dependencies:\n\
             \x20     - kind: service\n\
             \x20       services: [php]\n\
             \x20 php:\n\
             \x20   build:\n\
             \x20     context: .\n",
        )
        .unwrap();

        let graph = resolve_universe(tmp.path()).unwrap();

        assert!(graph.nodes.iter().any(|n| n.id == "vite.shop-web"));
        assert!(graph.nodes.iter().any(|n| n.id == "php.shop-web"));
        assert!(
            graph
                .edges
                .iter()
                .any(|e| e.from == "vite.shop-web" && e.to == "php.shop-web" && e.kind == "depends-on")
        );
        assert!(graph.warnings.is_empty());
    }

    #[test]
    fn git_dependency_without_repo_on_itself_warns_instead_of_crashing() {
        let tmp = tempfile::tempdir().unwrap();

        fs::create_dir_all(tmp.path().join("shop-web")).unwrap();
        fs::write(
            tmp.path().join("shop-web/fghj.yaml"),
            "version: \"1.0\"\n\
             services:\n\
             \x20 vite:\n\
             \x20   build:\n\
             \x20     context: .\n\
             \x20   dependencies:\n\
             \x20     - kind: service\n\
             \x20       services: [vite]\n",
        )
        .unwrap();

        let graph = resolve_universe(tmp.path()).unwrap();

        assert!(!graph.edges.iter().any(|e| e.kind == "depends-on"));
        assert!(graph.warnings.iter().any(|w| w.contains("itself")));
    }
}
