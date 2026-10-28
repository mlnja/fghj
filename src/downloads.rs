use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result, bail};

use crate::activity::{Activity, ActivityHandle, ActivityKind, ActivityLog};
use crate::docker;
use crate::persistence::WorkspaceOwner;
use crate::resolver::{self, BuildSource, Node};

/// fghjd's Docker, for the images a pull fetches. The pull runs on its own
/// thread, so it carries the runtime to drive the client with.
pub struct ImagePuller {
    pub docker: Arc<bollard::Docker>,
    pub runtime: tokio::runtime::Handle,
}

/// Clones `node_id`'s repo (or its build source) in the background, then
/// pulls its image if it runs one.
pub fn start_node(
    activities: &Arc<ActivityLog>,
    workspace_id: &str,
    workspace: PathBuf,
    node_id: String,
    owner: Option<WorkspaceOwner>,
    images: ImagePuller,
) -> Activity {
    spawn(
        activities,
        workspace_id,
        &format!("node:{node_id}"),
        move |job| {
            clone_node_logged(&workspace, &node_id, owner.as_ref(), job)?;
            let graph = resolver::resolve_universe(&workspace)?;
            let node: Vec<&Node> = graph.nodes.iter().filter(|n| n.id == node_id).collect();
            pull_images_logged(&node, &images, job)
        },
    )
}

/// `flow` scopes the pull and gets its own job key.
pub fn start_pull_all(
    activities: &Arc<ActivityLog>,
    workspace_id: &str,
    workspace: PathBuf,
    owner: Option<WorkspaceOwner>,
    flow: Option<String>,
    images: ImagePuller,
) -> Activity {
    spawn(
        activities,
        workspace_id,
        &pull_all_key(flow.as_deref()),
        move |job| {
            pull_all_logged(
                &workspace,
                owner.as_ref(),
                flow.as_deref(),
                Some(&images),
                job,
            )
        },
    )
}

/// Joins a pull already running under `key` rather than starting a second.
fn spawn(
    activities: &Arc<ActivityLog>,
    workspace_id: &str,
    key: &str,
    run: impl FnOnce(&ActivityHandle) -> Result<()> + Send + 'static,
) -> Activity {
    match activities.start(Some(workspace_id), ActivityKind::Pull, key) {
        Err(running) => *running,
        Ok(job) => {
            let snapshot = job.snapshot();
            thread::spawn(move || {
                let result = run(&job);
                job.settle(&result);
            });
            snapshot
        }
    }
}

pub fn pull_all_key(flow: Option<&str>) -> String {
    match flow {
        Some(flow) => format!("pull-flow:{flow}"),
        None => "pull-all".to_string(),
    }
}

fn stream_to_log(mut pipe: impl Read, job: ActivityHandle) {
    let mut buf = [0u8; 512];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buf[..n]).replace('\r', "\n");
                job.append(&chunk);
            }
            Err(_) => break,
        }
    }
}

/// Compared via `normalize_repo_url`. A folder with no readable `origin` can't
/// be vouched for, so it's an error too.
fn ensure_checkout_is(dest: &Path, repo: &str) -> Result<()> {
    let (origin, _) = resolver::git_remote_and_branch(dest);
    let Some(origin) = origin else {
        bail!(
            "{} already exists but is not a git checkout with an `origin` \
             remote; fghj cannot confirm it is {repo}. Remove or rename it, \
             then pull again.",
            dest.display()
        );
    };
    if resolver::normalize_repo_url(&origin) == resolver::normalize_repo_url(repo) {
        return Ok(());
    }
    bail!(
        "{} is a checkout of {origin}, not {repo}. Both repos want the same \
         workspace folder because it is named after the last segment of the \
         URL. fghj cannot hold both; rename or remove the existing checkout.",
        dest.display()
    )
}

fn run_git_clone_logged(
    workspace: &Path,
    repo: &str,
    branch: &str,
    local_path: &str,
    owner: Option<&WorkspaceOwner>,
    job: &ActivityHandle,
) -> Result<()> {
    let dest = workspace.join(local_path);
    if dest.exists() {
        // `org-a/api` and `org-b/api` both want `<ws>/api`; the resolver would read
        // the wrong `.fghj.yaml`, so refuse.
        return ensure_checkout_is(&dest, repo);
    }

    job.append(&format!(
        "$ git clone --branch {branch} {repo} {local_path}\n"
    ));

    let mut cmd = Command::new("git");
    cmd.args(["clone", "--progress", "--branch", branch, "--single-branch"])
        .arg(repo)
        .arg(&dest)
        // As the owner, for their credentials (see `WorkspaceOwner`).
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    run_git_logged(cmd, owner, job)
        .with_context(|| format!("git clone failed for {repo} (branch {branch})"))
}

fn run_git_logged(
    mut cmd: Command,
    owner: Option<&WorkspaceOwner>,
    job: &ActivityHandle,
) -> Result<()> {
    if let Some(owner) = owner {
        owner.apply_to_command(&mut cmd);
    }
    crate::persistence::harden_git_ssh(&mut cmd);

    let log_start = job.log_len();
    let mut child = cmd.spawn().context("failed to spawn git")?;

    let stderr = child.stderr.take().expect("piped stderr");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr_job = job.clone();
    let stdout_job = job.clone();
    let stderr_handle = thread::spawn(move || stream_to_log(stderr, stderr_job));
    let stdout_handle = thread::spawn(move || stream_to_log(stdout, stdout_job));

    let status = child.wait().context("failed waiting for git")?;
    let _ = stderr_handle.join();
    let _ = stdout_handle.join();

    if !status.success() {
        let output = job.log_from(log_start);
        if let Some(hint) = refusal_hint(&output, owner) {
            job.append(&format!("\nhint: {hint}\n"));
        }
        bail!("git exited with {status}");
    }
    Ok(())
}

/// For a git that ran as `owner` (or as fghjd, when `None`) and failed with
/// `output`.
pub(crate) fn refusal_hint(output: &str, owner: Option<&WorkspaceOwner>) -> Option<&'static str> {
    let as_root = unsafe { libc::geteuid() } == 0;
    let agent = match owner.map(WorkspaceOwner::live_ssh_auth_sock) {
        None | Some(None) => Agent::None,
        Some(Some(sock)) if crate::platform::current().session.is_session_agent(&sock) => {
            Agent::Session
        }
        Some(Some(_)) => Agent::Other,
    };
    failure_hint(output, owner.is_some(), as_root, agent)
}

/// Why a remote refused us, when the cause is how fghj ran git.
#[derive(Clone, Copy)]
enum Agent {
    None,
    /// The login session's, found by scanning. Keys kept in 1Password and the
    /// like aren't in it.
    Session,
    /// One `fghj wire` sent.
    Other,
}

fn failure_hint(
    output: &str,
    has_owner: bool,
    as_root: bool,
    agent: Agent,
) -> Option<&'static str> {
    let refused = [
        "Permission denied (publickey",
        "Authentication failed",
        "could not read Username",
    ]
    .iter()
    .any(|marker| output.contains(marker));
    if !refused {
        return None;
    }
    Some(match (has_owner, as_root, agent) {
        (false, true, _) => {
            "git ran as root, with root's keys: this workspace has no recorded owner and \
             its folder belongs to root. Run `fghj wire` from your own user, then pull again."
        }
        (false, false, _) => return None,
        (true, _, Agent::None) => {
            "git ran as you, with the keys in your ~/.ssh and no ssh-agent. A key with a \
             passphrase needs one: run `ssh-add`, then pull again. If your keys live in \
             another agent (1Password and the like), set `IdentityAgent` in ~/.ssh/config \
             or run `fghj wire` from your shell."
        }
        (true, _, Agent::Session) => {
            "git ran as you, with your login session's ssh-agent. If your keys live in another agent \
             (1Password and the like), set `IdentityAgent` in ~/.ssh/config or run \
             `fghj wire` from your shell. Otherwise check `ssh-add -l` lists your key."
        }
        (true, _, Agent::Other) => {
            "git ran as you, with your ssh-agent. If your key has a passphrase, check \
             `ssh-add -l` lists it; otherwise check your account can read this repository."
        }
    })
}

/// An existing clone is never fetched or reset, so pins and local patches stay.
fn clone_source_logged(
    workspace: &Path,
    source: &BuildSource,
    owner: Option<&WorkspaceOwner>,
    job: &ActivityHandle,
) -> Result<()> {
    let dest = workspace.join(&source.path);
    if dest.exists() {
        return ensure_checkout_is(&dest, &source.url);
    }
    // Made by root `fghjd`, so handed over before cloning into it as the owner.
    let sources = workspace.join(resolver::build_source::SOURCES_DIR);
    std::fs::create_dir_all(&sources)
        .with_context(|| format!("failed to create {}", sources.display()))?;
    if let Some(owner) = owner {
        owner
            .take(&sources)
            .with_context(|| format!("failed to hand {} to the owner", sources.display()))?;
    }

    let commit = source
        .reference
        .as_deref()
        .filter(|r| resolver::build_source::looks_like_commit(r));
    let mut cmd = Command::new("git");
    cmd.args(["clone", "--progress"]);
    match (&source.reference, commit) {
        (Some(_), Some(_)) | (None, _) => {}
        (Some(reference), None) => {
            cmd.args(["--branch", reference, "--single-branch"]);
        }
    }
    cmd.arg(&source.url)
        .arg(&dest)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    job.append(&format!(
        "$ git clone {}{} {}\n",
        match (&source.reference, commit) {
            (Some(r), None) => format!("--branch {r} "),
            _ => String::new(),
        },
        source.url,
        source.path
    ));
    run_git_logged(cmd, owner, job)
        .with_context(|| format!("git clone failed for {}", source.url))?;

    if let Some(commit) = commit {
        job.append(&format!("$ git checkout --detach {commit}\n"));
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&dest)
            .args(["checkout", "--detach", commit])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Err(e) = run_git_logged(cmd, owner, job) {
            // Left at the default branch it would build the wrong version.
            let _ = std::fs::remove_dir_all(&dest);
            return Err(e.context(format!("{} has no commit {commit}", source.url)));
        }
    }
    Ok(())
}

/// An `images` child holding one queued child per image a node in `nodes`
/// runs and the machine lacks, pulled in turn. Built images aren't here:
/// BuildKit fetches a build's base images itself.
fn pull_images_logged(nodes: &[&Node], puller: &ImagePuller, job: &ActivityHandle) -> Result<()> {
    let mut missing = Vec::new();
    for (reference, platform) in images_run_by(nodes) {
        if !puller
            .runtime
            .block_on(docker::image_is_local(&puller.docker, reference))?
        {
            missing.push((reference, platform));
        }
    }
    if missing.is_empty() {
        return Ok(());
    }

    let group = job.child("images");
    // All queued first, so the plan shows before the first one starts.
    let steps: Vec<ActivityHandle> = missing.iter().map(|(r, _)| group.queued_child(r)).collect();
    let mut result = Ok(());
    for ((reference, platform), step) in missing.iter().zip(&steps) {
        step.begin();
        step.append(&match platform {
            Some(platform) => format!("$ docker pull --platform {platform} {reference}\n"),
            None => format!("$ docker pull {reference}\n"),
        });
        let pulled = puller.runtime.block_on(docker::pull_image(
            &puller.docker,
            reference,
            *platform,
            &|line| step.append(line),
        ));
        step.settle(&pulled);
        if pulled.is_err() {
            result = pulled;
            break;
        }
    }
    group.settle(&result);
    result
}

/// Each image `nodes` run as is, once per platform. Built ones aren't here.
fn images_run_by<'a>(nodes: &[&'a Node]) -> Vec<(&'a str, Option<&'a str>)> {
    let mut images: Vec<(&str, Option<&str>)> = nodes
        .iter()
        .filter(|n| n.build.is_none())
        .filter_map(|n| Some((n.image.as_deref()?, n.platform.as_deref())))
        .collect();
    images.sort();
    images.dedup();
    images
}

fn missing_source(node: &Node) -> Option<&BuildSource> {
    node.build
        .as_ref()
        .and_then(|b| b.source.as_ref())
        .filter(|s| !s.downloaded)
}

fn clone_stub_logged(
    workspace: &Path,
    node: &Node,
    owner: Option<&WorkspaceOwner>,
    job: &ActivityHandle,
) -> Result<()> {
    let repo = node
        .repo
        .as_deref()
        .with_context(|| format!("stub node {} has no repo to clone", node.id))?;
    let branch = node.branch.as_deref().unwrap_or("main");
    let local_path = node.local_path.as_deref().unwrap_or(&node.id);
    run_git_clone_logged(workspace, repo, branch, local_path, owner, job)
}

fn clone_node_logged(
    workspace: &Path,
    node_id: &str,
    owner: Option<&WorkspaceOwner>,
    job: &ActivityHandle,
) -> Result<()> {
    let graph = resolver::resolve_universe(workspace)?;
    let node = graph
        .nodes
        .iter()
        .find(|n| n.id == node_id)
        .with_context(|| format!("no such node: {node_id}"))?;

    if let Some(source) = missing_source(node) {
        return clone_source_logged(workspace, source, owner, job);
    }
    if node.downloaded {
        job.append("already downloaded\n");
        return Ok(());
    }

    clone_stub_logged(workspace, node, owner, job)
}

/// Clones layer by layer until nothing is missing, then pulls the images
/// the wanted nodes run. `images` is `None` in tests, which only clone.
fn pull_all_logged(
    workspace: &Path,
    owner: Option<&WorkspaceOwner>,
    flow: Option<&str>,
    images: Option<&ImagePuller>,
    job: &ActivityHandle,
) -> Result<()> {
    std::fs::create_dir_all(workspace)
        .with_context(|| format!("failed to create workspace dir {}", workspace.display()))?;

    loop {
        let graph = resolver::resolve_universe(workspace)?;
        let wanted = graph.start_ids(flow)?;
        let wanted: Vec<&Node> = graph
            .nodes
            .iter()
            .filter(|n| wanted.contains(&n.id))
            .collect();
        let missing: Vec<&Node> = wanted.iter().copied().filter(|n| !n.downloaded).collect();
        let mut sources: Vec<&BuildSource> =
            wanted.iter().filter_map(|n| missing_source(n)).collect();
        sources.sort_by(|a, b| a.path.cmp(&b.path));
        sources.dedup_by(|a, b| a.path == b.path);

        if missing.is_empty() && sources.is_empty() {
            if let Some(images) = images {
                pull_images_logged(&wanted, images, job)?;
            }
            job.append("\nnothing left to pull\n");
            return Ok(());
        }

        // One child per clone, keyed by where it lands.
        for source in sources {
            let clone = job.child(&source.path);
            let result = clone_source_logged(workspace, source, owner, &clone);
            clone.settle(&result);
            result?;
        }
        for node in missing {
            let clone = job.child(node.local_path.as_deref().unwrap_or(&node.id));
            let result = clone_stub_logged(workspace, node, owner, &clone);
            clone.settle(&result);
            result?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_while_cloning_as_root_says_to_wire() {
        let output = "git@gitlab.com: Permission denied (publickey).\nfatal: Could not read";
        let hint = failure_hint(output, false, true, Agent::None).unwrap();
        assert!(hint.contains("fghj wire"), "{hint}");
        assert!(
            failure_hint(output, true, true, Agent::None)
                .unwrap()
                .contains("ssh-add")
        );
    }

    #[test]
    fn a_refusal_through_the_session_agent_points_at_other_agents() {
        let output = "git@gitlab.com: Permission denied (publickey).";
        let hint = failure_hint(output, true, true, Agent::Session).unwrap();
        assert!(hint.contains("IdentityAgent"), "{hint}");
        let hint = failure_hint(output, true, true, Agent::Other).unwrap();
        assert!(!hint.contains("IdentityAgent"), "{hint}");
    }

    #[test]
    fn other_failures_get_no_hint() {
        let output = "fatal: repository 'x' not found";
        assert_eq!(failure_hint(output, false, true, Agent::None), None);
    }

    fn checkout_with_origin(origin: Option<&str>) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let ok = Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .arg("init")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git init failed");
        if let Some(origin) = origin {
            let ok = Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(["remote", "add", "origin", origin])
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git remote add failed");
        }
        tmp
    }

    #[test]
    fn an_existing_checkout_of_the_same_repo_is_accepted() {
        let tmp = checkout_with_origin(Some("https://github.com/org-a/api.git"));
        ensure_checkout_is(tmp.path(), "https://github.com/org-a/api.git").unwrap();
    }

    #[test]
    fn spelling_of_the_url_does_not_matter() {
        let tmp = checkout_with_origin(Some("git@github.com:org-a/api.git"));
        ensure_checkout_is(tmp.path(), "https://github.com/org-a/api").unwrap();
    }

    #[test]
    fn a_different_repo_in_the_same_folder_is_refused() {
        let tmp = checkout_with_origin(Some("https://github.com/org-a/api.git"));
        let err = ensure_checkout_is(tmp.path(), "https://github.com/org-b/api.git")
            .unwrap_err()
            .to_string();
        assert!(err.contains("org-a"), "{err}");
        assert!(err.contains("org-b"), "{err}");
    }

    #[test]
    fn a_folder_with_no_origin_is_refused_rather_than_assumed() {
        let tmp = checkout_with_origin(None);
        let err = ensure_checkout_is(tmp.path(), "https://github.com/org-a/api.git")
            .unwrap_err()
            .to_string();
        assert!(err.contains("origin"), "{err}");
    }

    #[test]
    fn a_plain_directory_that_is_not_a_checkout_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(ensure_checkout_is(tmp.path(), "https://github.com/org-a/api.git").is_err());
    }

    mod pull {
        use super::super::*;
        use std::fs;

        /// `{remotes}` in a config becomes the remotes' path.
        struct World {
            root: tempfile::TempDir,
        }

        fn git(dir: &Path, args: &[&str]) {
            let out = Command::new("git")
                .args([
                    "-c",
                    "user.name=fghj",
                    "-c",
                    "user.email=fghj@example.com",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }

        impl World {
            fn new() -> Self {
                let root = tempfile::tempdir().unwrap();
                fs::create_dir_all(root.path().join("ws")).unwrap();
                fs::create_dir_all(root.path().join("remotes")).unwrap();
                World { root }
            }

            fn ws(&self) -> PathBuf {
                self.root.path().join("ws")
            }

            fn url(&self, name: &str) -> String {
                self.root
                    .path()
                    .join("remotes")
                    .join(format!("{name}.git"))
                    .display()
                    .to_string()
            }

            fn remote(&self, name: &str, yaml: &str) -> &Self {
                let src = self.root.path().join("src").join(name);
                fs::create_dir_all(&src).unwrap();
                let remotes = self.root.path().join("remotes").display().to_string();
                fs::write(src.join(".fghj.yaml"), yaml.replace("{remotes}", &remotes)).unwrap();
                git(&src, &["init", "-q", "-b", "main"]);
                git(&src, &["add", "-f", "."]);
                git(&src, &["commit", "-q", "-m", "init"]);
                git(
                    self.root.path(),
                    &[
                        "clone",
                        "-q",
                        "--bare",
                        &src.display().to_string(),
                        &self.url(name),
                    ],
                );
                self
            }

            fn checkout(&self, name: &str) -> &Self {
                git(&self.ws(), &["clone", "-q", &self.url(name), name]);
                self
            }

            fn pull(&self, flow: Option<&str>) -> Result<Vec<String>> {
                let activities = Arc::new(ActivityLog::default());
                let job = activities
                    .start(Some("test"), ActivityKind::Pull, "pull-all")
                    .unwrap();
                let result = pull_all_logged(&self.ws(), None, flow, None, &job);
                let log = job.snapshot().log;
                result.map_err(|e| anyhow::anyhow!("{e}\nlog:\n{log}"))?;
                Ok(log
                    .lines()
                    .filter(|l| l.starts_with("$ git clone"))
                    .map(|l| l.rsplit(' ').next().unwrap().to_string())
                    .collect())
            }

            fn on_disk(&self, name: &str) -> bool {
                self.ws().join(name).join(".fghj.yaml").exists()
            }

            fn graph(&self) -> resolver::Graph {
                resolver::resolve_universe(&self.ws()).unwrap()
            }
        }

        const SHOP: &str = r#"version: "2.0"
include:
  billing: {remotes}/billing.git
  analytics: {remotes}/analytics.git
services:
  web:
    build: .
    depends_on: [billing/pricing]
flows:
  checkout: [web]
  reports: [analytics]
"#;

        const BILLING: &str = r#"version: "2.0"
include:
  ledger: {remotes}/ledger.git
  search: {remotes}/search.git
services:
  api:
    build: .
    depends_on: [ledger/core]
  indexer:
    build: .
    depends_on: [search]
flows:
  pricing: [api]
"#;

        const LEDGER: &str = r#"version: "2.0"
services:
  ledger:
    build: .
flows:
  core: [ledger]
"#;

        const LEAF: &str = "version: \"2.0\"\nservices:\n  svc:\n    image: busybox\n";

        fn world() -> World {
            let w = World::new();
            w.remote("shop", SHOP)
                .remote("billing", BILLING)
                .remote("ledger", LEDGER)
                .remote("search", LEAF)
                .remote("analytics", LEAF)
                .checkout("shop");
            w
        }

        #[test]
        fn pull_all_clones_layer_by_layer_until_nothing_is_missing() {
            let w = world();
            let cloned = w.pull(None).unwrap();
            assert_eq!(cloned.len(), 4, "{cloned:?}");
            let pos = |n: &str| cloned.iter().position(|c| c == n).unwrap();
            for first in ["billing", "analytics"] {
                for second in ["ledger", "search"] {
                    assert!(pos(first) < pos(second), "{cloned:?}");
                }
            }
            let graph = w.graph();
            assert!(
                graph.nodes.iter().all(|n| n.downloaded),
                "{:?}",
                graph.nodes
            );
            assert!(graph.warnings.is_empty(), "{:#?}", graph.warnings);
        }

        #[test]
        fn a_flow_pulls_only_what_it_would_start() {
            let w = world();
            assert_eq!(
                w.pull(Some("shop/checkout")).unwrap(),
                ["billing", "ledger"]
            );
            assert!(!w.on_disk("analytics"));
            assert!(!w.on_disk("search"));

            let rest = w.pull(None).unwrap();
            let mut rest_sorted = rest.clone();
            rest_sorted.sort();
            assert_eq!(rest_sorted, ["analytics", "search"], "{rest:?}");
        }

        fn tag(w: &World, name: &str, tag: &str) -> String {
            let bare = PathBuf::from(w.url(name));
            git(&bare, &["-c", "tag.gpgsign=false", "tag", tag, "main"]);
            let out = Command::new("git")
                .args(["rev-parse", "main"])
                .current_dir(&bare)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        fn sourced_shop(w: &World, reference: &str) {
            w.remote(
                "shop",
                &format!(
                    r#"version: "2.0"
services:
  web:
    build: .
  geocoder:
    build:
      context: file://{{remotes}}/geocoder.git#{reference}
      dockerfile_inline: FROM scratch
  geocoder-worker:
    build:
      context: file://{{remotes}}/geocoder.git#{reference}
      dockerfile_inline: FROM scratch
flows:
  maps: [geocoder, geocoder-worker]
  front: [web]
"#
                ),
            )
            .checkout("shop");
        }

        fn source_of(graph: &resolver::Graph, id: &str) -> BuildSource {
            graph
                .nodes
                .iter()
                .find(|n| n.id == id)
                .and_then(|n| n.build.as_ref()?.source.clone())
                .unwrap()
        }

        #[test]
        fn a_pull_clones_the_sources_of_what_it_would_start() {
            let w = World::new();
            w.remote("geocoder", LEAF);
            let commit = tag(&w, "geocoder", "v1");
            sourced_shop(&w, "v1");

            assert!(w.pull(Some("shop/front")).unwrap().is_empty());
            assert!(!source_of(&w.graph(), "geocoder.shop").downloaded);

            assert_eq!(
                w.pull(Some("shop/maps")).unwrap(),
                ["_fghj_sources/geocoder@v1"]
            );
            let graph = w.graph();
            let source = source_of(&graph, "geocoder.shop");
            assert!(source.downloaded);
            assert_eq!(source.head.as_deref(), Some(commit.as_str()));
            assert!(!source.dirty);
            assert!(graph.nodes.iter().all(|n| !n.id.ends_with(".geocoder")));

            assert!(w.pull(None).unwrap().is_empty());
        }

        #[test]
        fn a_commit_ref_is_checked_out() {
            let w = World::new();
            w.remote("geocoder", LEAF);
            let commit = tag(&w, "geocoder", "v1");
            sourced_shop(&w, &commit[..12]);

            w.pull(None).unwrap();
            let source = source_of(&w.graph(), "geocoder.shop");
            assert_eq!(source.head.as_deref(), Some(commit.as_str()));
        }

        #[test]
        fn a_ref_the_remote_lacks_fails_the_pull_and_leaves_nothing() {
            let w = World::new();
            w.remote("geocoder", LEAF);
            sourced_shop(&w, "v9");

            assert!(w.pull(None).is_err());
            assert!(!w.ws().join("_fghj_sources/geocoder@v9").exists());
        }

        #[test]
        fn a_second_pull_clones_nothing() {
            let w = world();
            w.pull(None).unwrap();
            assert!(w.pull(None).unwrap().is_empty());
        }

        #[test]
        fn a_repo_two_others_include_is_cloned_once() {
            let w = World::new();
            let includes_ledger = r#"version: "2.0"
include:
  ledger: {remotes}/ledger.git
services:
  svc:
    build: .
    depends_on: [ledger/core]
"#;
            w.remote("left", includes_ledger)
                .remote("right", includes_ledger)
                .remote("ledger", LEDGER)
                .remote(
                    "top",
                    r#"version: "2.0"
include:
  left: {remotes}/left.git
  right: {remotes}/right.git
services:
  svc:
    build: .
    depends_on: [left, right]
"#,
                )
                .checkout("top");
            let cloned = w.pull(None).unwrap();
            assert_eq!(
                cloned.iter().filter(|c| *c == "ledger").count(),
                1,
                "{cloned:?}"
            );
            assert_eq!(
                cloned.last().map(String::as_str),
                Some("ledger"),
                "{cloned:?}"
            );
        }

        #[test]
        fn mutual_includes_terminate() {
            let w = World::new();
            w.remote(
                "a",
                r#"version: "2.0"
include:
  b: {remotes}/b.git
services:
  x:
    build: .
    depends_on: [b]
"#,
            )
            .remote(
                "b",
                r#"version: "2.0"
include:
  a: {remotes}/a.git
services:
  y:
    build: .
    environment:
      A: http://${FGHJ_SERVICE_FQDN:a/x}
"#,
            )
            .checkout("a");
            assert_eq!(w.pull(None).unwrap(), ["b"]);
            assert!(w.graph().warnings.is_empty(), "{:#?}", w.graph().warnings);
        }

        #[test]
        fn an_unreachable_remote_fails_the_pull() {
            let w = World::new();
            w.remote(
                "a",
                r#"version: "2.0"
include:
  gone: {remotes}/gone.git
services:
  x:
    build: .
    depends_on: [gone]
"#,
            )
            .checkout("a");
            let err = w.pull(None).unwrap_err().to_string();
            assert!(err.contains("gone"), "{err}");
        }

        #[test]
        fn only_images_run_as_is_are_pulled_once_each() {
            let world = World::new();
            world
                .remote(
                    "shop",
                    "version: \"2.0\"\nservices:\n  \
                     db:\n    image: postgres:16-alpine\n  \
                     replica:\n    image: postgres:16-alpine\n  \
                     arm:\n    image: postgres:16-alpine\n    platform: linux/arm64\n  \
                     web:\n    build: .\n",
                )
                .checkout("shop");
            let graph = world.graph();
            let nodes: Vec<&Node> = graph.nodes.iter().collect();
            assert_eq!(
                images_run_by(&nodes),
                vec![
                    ("postgres:16-alpine", None),
                    ("postgres:16-alpine", Some("linux/arm64")),
                ]
            );
        }

        #[test]
        fn pulling_an_unknown_flow_clones_nothing() {
            let w = world();
            assert!(w.pull(Some("shop/nope")).is_err());
            assert!(!w.on_disk("billing"));
        }
    }
}
