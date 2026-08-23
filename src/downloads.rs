use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::resolver::{self, Node};

#[derive(Debug, Serialize, Clone)]
pub struct DownloadState {
    pub status: String, // "running" | "done" | "error"
    pub log: String,
}

/// Tracks background `git clone` jobs kicked off from the UI, keyed by
/// `"node:<id>"` for a single-node download or `"pull-all"` for the
/// fixpoint pull-everything job, so the UI can poll for live progress
/// instead of blocking the request until the clone finishes.
#[derive(Default)]
pub struct DownloadRegistry {
    jobs: Mutex<BTreeMap<String, Arc<Mutex<DownloadState>>>>,
}

impl DownloadRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn status(&self, key: &str) -> Option<DownloadState> {
        self.jobs
            .lock()
            .unwrap()
            .get(key)
            .map(|s| s.lock().unwrap().clone())
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
        if let Some(existing) = jobs.get(&key) {
            let snapshot = existing.lock().unwrap().clone();
            if snapshot.status == "running" {
                return snapshot;
            }
        }
        let state = Arc::new(Mutex::new(DownloadState {
            status: "running".to_string(),
            log: String::new(),
        }));
        jobs.insert(key, state.clone());
        let snapshot = state.lock().unwrap().clone();
        thread::spawn(move || run(state));
        snapshot
    }

    pub fn start_node(&self, workspace: PathBuf, node_id: String) -> DownloadState {
        let key = format!("node:{node_id}");
        self.spawn(key, move |state| {
            let result = clone_node_logged(&workspace, &node_id, &state);
            finish(&state, result);
        })
    }

    pub fn start_pull_all(&self, workspace: PathBuf) -> DownloadState {
        self.spawn("pull-all".to_string(), move |state| {
            let result = pull_all_logged(&workspace, &state);
            finish(&state, result);
        })
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
    state: &Arc<Mutex<DownloadState>>,
) -> Result<()> {
    let dest = workspace.join(local_path);
    if dest.exists() {
        return Ok(());
    }

    append_log(state, &format!("$ git clone --branch {branch} {repo} {local_path}\n"));

    let mut child = Command::new("git")
        .args(["clone", "--progress", "--branch", branch, "--single-branch"])
        .arg(repo)
        .arg(&dest)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn git clone for {repo}"))?;

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

fn clone_stub_logged(workspace: &Path, node: &Node, state: &Arc<Mutex<DownloadState>>) -> Result<()> {
    let repo = node
        .repo
        .as_deref()
        .with_context(|| format!("stub node {} has no repo to clone", node.id))?;
    let branch = node.branch.as_deref().unwrap_or("main");
    let local_path = node.local_path.as_deref().unwrap_or(&node.id);
    run_git_clone_logged(workspace, repo, branch, local_path, state)
}

fn clone_node_logged(workspace: &Path, node_id: &str, state: &Arc<Mutex<DownloadState>>) -> Result<()> {
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

    clone_stub_logged(workspace, node, state)
}

fn pull_all_logged(workspace: &Path, state: &Arc<Mutex<DownloadState>>) -> Result<()> {
    std::fs::create_dir_all(workspace)
        .with_context(|| format!("failed to create workspace dir {}", workspace.display()))?;

    loop {
        let graph = resolver::resolve_universe(workspace)?;
        let missing: Vec<Node> = graph
            .nodes
            .into_iter()
            .filter(|n| n.kind == "service" && !n.downloaded)
            .collect();

        if missing.is_empty() {
            if missing.is_empty() {
                append_log(state, "\nnothing left to pull\n");
            }
            return Ok(());
        }

        for node in &missing {
            clone_stub_logged(workspace, node, state)?;
        }
    }
}
