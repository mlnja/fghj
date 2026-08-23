use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::resolver::{self, Node};
use crate::store::WorkspaceOwner;

#[derive(Debug, Serialize, Clone)]
pub struct DownloadState {
    pub status: String, // "running" | "done" | "error"
    pub log: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct DownloadJob {
    pub key: String,
    pub status: String,
    pub log: String,
}

/// Tracks background `git clone` jobs kicked off from the UI, keyed by
/// `"node:<id>"` for a single-node download or `"pull-all"` for the
/// fixpoint pull-everything job, so the UI can poll for live progress
/// instead of blocking the request until the clone finishes.
///
/// Backed by a `Vec` rather than a map so iteration order reflects the order
/// jobs were first started (a "queue"), not key sort order — the operations
/// drawer in the UI lists jobs in this order.
#[derive(Default)]
pub struct DownloadRegistry {
    jobs: Mutex<Vec<(String, Arc<Mutex<DownloadState>>)>>,
}

impl DownloadRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn status(&self, key: &str) -> Option<DownloadState> {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, s)| s.lock().unwrap().clone())
    }

    /// All known jobs, most-recently-started first, for the operations queue view.
    pub fn list(&self) -> Vec<DownloadJob> {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .rev()
            .map(|(key, s)| {
                let snapshot = s.lock().unwrap();
                DownloadJob { key: key.clone(), status: snapshot.status.clone(), log: snapshot.log.clone() }
            })
            .collect()
    }

    /// Starts `run` in a background thread under `key`, unless a job with
    /// that key is already running. Returns the (possibly pre-existing)
    /// state so the caller can respond immediately.
    fn spawn(
        &self,
        key: String,
        run: impl FnOnce(Arc<Mutex<DownloadState>>) + Send + 'static,
    ) -> DownloadState {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some((_, existing)) = jobs.iter().find(|(k, _)| *k == key) {
            let snapshot = existing.lock().unwrap().clone();
            if snapshot.status == "running" {
                return snapshot;
            }
        }
        let state = Arc::new(Mutex::new(DownloadState {
            status: "running".to_string(),
            log: String::new(),
        }));
        if let Some(slot) = jobs.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = state.clone();
        } else {
            jobs.push((key, state.clone()));
        }
        let snapshot = state.lock().unwrap().clone();
        thread::spawn(move || run(state));
        snapshot
    }

    pub fn start_node(&self, workspace: PathBuf, node_id: String, owner: Option<WorkspaceOwner>) -> DownloadState {
        let key = format!("node:{node_id}");
        self.spawn(key, move |state| {
            let result = clone_node_logged(&workspace, &node_id, owner.as_ref(), &state);
            finish(&state, result);
        })
    }

    /// `flow`, when given, scopes the pull to nodes reachable from that flow
    /// (see `Node::flows`) instead of the whole graph, and tracks it under
    /// its own queue entry (`pull-flow:<flow>`) so a flow-scoped pull and the
    /// whole-graph "Pull all" can run and be polled independently.
    pub fn start_pull_all(&self, workspace: PathBuf, owner: Option<WorkspaceOwner>, flow: Option<String>) -> DownloadState {
        let key = pull_all_key(flow.as_deref());
        self.spawn(key, move |state| {
            let result = pull_all_logged(&workspace, owner.as_ref(), flow.as_deref(), &state);
            finish(&state, result);
        })
    }
}

/// The `DownloadRegistry` job key for a "pull all" run, shared by
/// `start_pull_all` and the daemon's status-lookup handler so both agree on
/// how a flow name turns into a key.
pub fn pull_all_key(flow: Option<&str>) -> String {
    match flow {
        Some(flow) => format!("pull-flow:{flow}"),
        None => "pull-all".to_string(),
    }
}

fn finish(state: &Arc<Mutex<DownloadState>>, result: Result<()>) {
    let mut s = state.lock().unwrap();
    match result {
        Ok(()) => s.status = "done".to_string(),
        Err(e) => {
            s.log.push_str(&format!("\nerror: {e}\n"));
            s.status = "error".to_string();
        }
    }
}

fn append_log(state: &Arc<Mutex<DownloadState>>, text: &str) {
    state.lock().unwrap().log.push_str(text);
}

fn stream_to_log(mut pipe: impl Read, state: Arc<Mutex<DownloadState>>) {
    let mut buf = [0u8; 512];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // git's progress meter uses \r to overwrite a line in a real
                // terminal; rendered in a <pre> block \n reads better.
                let chunk = String::from_utf8_lossy(&buf[..n]).replace('\r', "\n");
                append_log(&state, &chunk);
            }
            Err(_) => break,
        }
    }
}

fn run_git_clone_logged(
    workspace: &Path,
    repo: &str,
    branch: &str,
    local_path: &str,
    owner: Option<&WorkspaceOwner>,
    state: &Arc<Mutex<DownloadState>>,
) -> Result<()> {
    let dest = workspace.join(local_path);
    if dest.exists() {
        return Ok(());
    }

    append_log(state, &format!("$ git clone --branch {branch} {repo} {local_path}\n"));

    let mut cmd = Command::new("git");
    cmd.args(["clone", "--progress", "--branch", branch, "--single-branch"])
        .arg(repo)
        .arg(&dest)
        // fghjd runs as root (via sudo), which has no credentials of its own
        // for a user's private remotes. If we know which real user wired
        // this workspace (captured by the unprivileged `fghj` CLI at `wire`
        // time), drop the child back to that user so it picks up their own
        // known_hosts, git config, and ssh-agent — root bypasses the usual
        // file permission checks on the agent's unix socket, so this works
        // even though the socket is owned by that user, not root.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(owner) = owner {
        owner.apply_to_command(&mut cmd);
    }
    crate::store::harden_git_ssh(&mut cmd);

    let mut child = cmd.spawn().with_context(|| format!("failed to spawn git clone for {repo}"))?;

    // git clone writes its progress meter to stderr, not stdout.
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr_state = state.clone();
    let stdout_state = state.clone();
    let stderr_handle = thread::spawn(move || stream_to_log(stderr, stderr_state));
    let stdout_handle = thread::spawn(move || stream_to_log(stdout, stdout_state));

    let status = child
        .wait()
        .with_context(|| format!("failed waiting for git clone of {repo}"))?;
    let _ = stderr_handle.join();
    let _ = stdout_handle.join();

    if !status.success() {
        bail!("git clone failed for {repo} (branch {branch})");
    }
    Ok(())
}

fn clone_stub_logged(
    workspace: &Path,
    node: &Node,
    owner: Option<&WorkspaceOwner>,
    state: &Arc<Mutex<DownloadState>>,
) -> Result<()> {
    let repo = node
        .repo
        .as_deref()
        .with_context(|| format!("stub node {} has no repo to clone", node.id))?;
    let branch = node.branch.as_deref().unwrap_or("main");
    let local_path = node.local_path.as_deref().unwrap_or(&node.id);
    run_git_clone_logged(workspace, repo, branch, local_path, owner, state)
}

fn clone_node_logged(
    workspace: &Path,
    node_id: &str,
    owner: Option<&WorkspaceOwner>,
    state: &Arc<Mutex<DownloadState>>,
) -> Result<()> {
    let graph = resolver::resolve_universe(workspace)?;
    let node = graph
        .nodes
        .iter()
        .find(|n| n.id == node_id)
        .with_context(|| format!("no such node: {node_id}"))?;

    if node.downloaded {
        append_log(state, "already downloaded\n");
        return Ok(());
    }

    clone_stub_logged(workspace, node, owner, state)
}

fn pull_all_logged(
    workspace: &Path,
    owner: Option<&WorkspaceOwner>,
    flow: Option<&str>,
    state: &Arc<Mutex<DownloadState>>,
) -> Result<()> {
    std::fs::create_dir_all(workspace)
        .with_context(|| format!("failed to create workspace dir {}", workspace.display()))?;

    loop {
        let graph = resolver::resolve_universe(workspace)?;
        let missing: Vec<Node> = graph
            .nodes
            .into_iter()
            .filter(|n| n.kind == "service" && !n.downloaded)
            .filter(|n| flow.is_none_or(|flow| n.flows.iter().any(|f| f == flow)))
            .collect();

        if missing.is_empty() {
            append_log(state, "\nnothing left to pull\n");
            return Ok(());
        }

        for node in &missing {
            clone_stub_logged(workspace, node, owner, state)?;
        }
    }
}
