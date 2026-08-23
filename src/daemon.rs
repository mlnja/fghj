use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tiny_http::Method;

use crate::server::{self, WorkspaceState};
use crate::{resolver, runs, store};

pub const CONTROL_PORT: u16 = 7880;

/// How often the background reconciler re-inspects live containers. Kept in
/// step with the frontend's `/runs` poll interval (see `App.svelte`) so the
/// UI is essentially never stale.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);

pub fn pid_path() -> PathBuf {
    PathBuf::from("/var/run/fghjd.pid")
}

pub fn write_pid(pid: u32) -> Result<()> {
    std::fs::write(pid_path(), pid.to_string())
        .with_context(|| format!("failed to write pidfile {}", pid_path().display()))
}

pub fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_path()).ok()?.trim().parse().ok()
}

pub fn remove_pid() {
    let _ = std::fs::remove_file(pid_path());
}

/// Checks whether a process with the given pid is alive, via a signal-0 kill.
/// `fghjd` runs as root while this is called from the unprivileged `fghj`
/// client, so a live process shows up as `EPERM` (exists, not ours to
/// signal), not `0` — only `ESRCH` means it's actually gone.
pub fn pid_alive(pid: u32) -> bool {
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Deterministic, URL-safe id for a canonicalized workspace path (FNV-1a of
/// the path, prefixed with a readable slug of its directory name), so
/// repeated `fghj ui` calls against the same directory land on the same
/// workspace instead of registering a duplicate.
fn workspace_id(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("workspace");
    let slug: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    format!("{slug}-{hash:012x}")
}

/// In-memory registry of workspaces the daemon knows about, keyed by id.
/// Holds only data (path + per-workspace registries) — there is no thread or
/// listener tied to a workspace; all of them are served off the single
/// control-port accept loop.
pub struct WorkspaceRegistry {
    by_id: Mutex<HashMap<String, Arc<WorkspaceState>>>,
    index_path: PathBuf,
}

impl WorkspaceRegistry {
    /// Rebuilds the registry from the central workspace index at the real,
    /// root-owned path. `load_from` does the actual work — split out so
    /// tests can point the index at a tempdir instead.
    pub fn load() -> Self {
        Self::load_from(store::default_index_path())
    }

    fn load_from(index_path: PathBuf) -> Self {
        let mut by_id = HashMap::new();
        for (id, path) in store::load_index(&index_path) {
            if !path.exists() {
                eprintln!("fghjd: skipping missing workspace {id} ({})", path.display());
                continue;
            }
            match WorkspaceState::new(path.clone()) {
                Ok(state) => {
                    by_id.insert(id, Arc::new(state));
                }
                Err(e) => eprintln!("fghjd: failed to load workspace {id} ({}): {e}", path.display()),
            }
        }
        Self { by_id: Mutex::new(by_id), index_path }
    }

    /// Resolves (cloning `entry` if needed) and registers a workspace,
    /// reusing the existing entry if this path is already known. Errors if
    /// the path is nested inside an already-wired workspace — a workspace
    /// root covers its whole subtree, so a second registration underneath it
    /// would just be an alias for part of the same tree.
    pub fn resolve(&self, entry: Option<String>, workspace: Option<PathBuf>) -> Result<(String, PathBuf)> {
        let entry_for_meta = entry.clone();
        let path = crate::resolve_workspace(entry, workspace)?;
        let canonical = std::fs::canonicalize(&path).unwrap_or(path);

        let mut by_id = self.by_id.lock().unwrap();
        for state in by_id.values() {
            if canonical != state.path && canonical.starts_with(&state.path) {
                bail!(
                    "{} is inside the already-wired workspace {}",
                    canonical.display(),
                    state.path.display()
                );
            }
        }

        let id = workspace_id(&canonical);
        if !by_id.contains_key(&id) {
            let state = WorkspaceState::new(canonical.clone())?;
            state.db.record_meta(&id, entry_for_meta.as_deref())?;
            by_id.insert(id.clone(), Arc::new(state));

            let mut index = store::load_index(&self.index_path);
            index.insert(id.clone(), canonical.clone());
            store::save_index(&self.index_path, &index)?;
        }
        Ok((id, canonical))
    }

    pub fn get(&self, id: &str) -> Option<Arc<WorkspaceState>> {
        self.by_id.lock().unwrap().get(id).cloned()
    }

    pub fn list(&self) -> Vec<(String, PathBuf)> {
        self.by_id.lock().unwrap().iter().map(|(id, s)| (id.clone(), s.path.clone())).collect()
    }

    /// Stops every live run in the workspace, forgets it in memory, and
    /// drops it from the central index so a future `fghjd` restart doesn't
    /// bring it back.
    pub fn stop(&self, id: &str) -> bool {
        let removed = self.by_id.lock().unwrap().remove(id);
        match removed {
            Some(state) => {
                for run in state.runs.list() {
                    let _ = state.runs.stop(&run.run_id);
                }
                let mut index = store::load_index(&self.index_path);
                index.remove(id);
                let _ = store::save_index(&self.index_path, &index);
                true
            }
            None => false,
        }
    }
}

#[derive(Deserialize)]
struct StartRequest {
    entry: Option<String>,
    workspace: Option<PathBuf>,
}

#[derive(Deserialize)]
struct StopRequest {
    id: String,
}

/// Runs `f` against the workspace named by `?workspace=<id>` in `query`, or
/// returns a 400 if the query param is missing or names an unknown id.
fn with_workspace(
    registry: &WorkspaceRegistry,
    query: &str,
    f: impl FnOnce(&WorkspaceState) -> (Vec<u8>, &'static str, u16),
) -> (Vec<u8>, &'static str, u16) {
    match server::query_param(query, "workspace").and_then(|id| registry.get(id)) {
        Some(state) => f(&state),
        None => server::missing_workspace_response(),
    }
}

/// Background loop, analogous to a Kubernetes controller's reconcile loop
/// but read-only: on each tick it re-inspects every workspace's live
/// containers and updates their recorded status (see `RunRegistry::refresh`)
/// so drift caused by someone `docker stop`/`rm`-ing a container by hand
/// shows up in the UI on its own, without a `fghjd` restart. It never
/// recreates or restarts anything — no self-healing, purely observational.
fn spawn_reconciler(registry: Arc<WorkspaceRegistry>) {
    thread::spawn(move || loop {
        thread::sleep(RECONCILE_INTERVAL);
        for (id, _) in registry.list() {
            if let Some(state) = registry.get(&id) {
                state.runs.refresh();
            }
        }
    });
}

/// Binds the control API on `CONTROL_PORT` and serves it forever, spawning a
/// thread per incoming request so a slow operation in one workspace (e.g. a
/// docker build) can't stall polling for every other workspace.
pub fn run_control_api() -> Result<()> {
    let registry = Arc::new(WorkspaceRegistry::load());
    spawn_reconciler(Arc::clone(&registry));
    let http = tiny_http::Server::http(("127.0.0.1", CONTROL_PORT))
        .map_err(|e| anyhow::anyhow!("failed to bind control API on port {CONTROL_PORT}: {e}"))?;

    for request in http.incoming_requests() {
        let registry = Arc::clone(&registry);
        thread::spawn(move || handle_request(request, &registry));
    }

    Ok(())
}

fn handle_request(mut request: tiny_http::Request, registry: &WorkspaceRegistry) {
    let full_path = request.url().to_string();
    let route = full_path.split('?').next().unwrap_or("/").to_string();
    let query = full_path.split_once('?').map(|(_, q)| q.to_string()).unwrap_or_default();
    let method = request.method().clone();
    let segments: Vec<&str> = route.trim_matches('/').split('/').collect();

    let response: (Vec<u8>, &str, u16) = match (&method, segments.as_slice()) {
        (Method::Get, ["workspaces"]) => {
            let list: Vec<_> = registry
                .list()
                .into_iter()
                .map(|(id, workspace)| serde_json::json!({ "id": id, "workspace": workspace }))
                .collect();
            server::json_response(serde_json::json!(list), 200)
        }
        (Method::Post, ["workspaces"]) => {
            let mut buf = String::new();
            let _ = request.as_reader().read_to_string(&mut buf);
            match serde_json::from_str::<StartRequest>(&buf) {
                Ok(req) => match registry.resolve(req.entry, req.workspace) {
                    Ok((id, workspace)) => server::json_response(serde_json::json!({ "id": id, "workspace": workspace }), 200),
                    Err(e) => server::err_response(e),
                },
                Err(e) => server::json_response(serde_json::json!({ "error": e.to_string() }), 400),
            }
        }
        (Method::Post, ["workspaces", "stop"]) => {
            let mut buf = String::new();
            let _ = request.as_reader().read_to_string(&mut buf);
            match serde_json::from_str::<StopRequest>(&buf) {
                Ok(req) => server::json_response(serde_json::json!({ "stopped": registry.stop(&req.id) }), 200),
                Err(e) => server::json_response(serde_json::json!({ "error": e.to_string() }), 400),
            }
        }
        (Method::Get, ["universe.json"]) => with_workspace(registry, &query, |state| {
            match resolver::resolve_universe(&state.path) {
                Ok(g) => server::json_response(serde_json::json!(g), 200),
                Err(e) => server::err_response(e),
            }
        }),
        (Method::Post, ["pull-all"]) => with_workspace(registry, &query, |state| {
            let s = state.downloads.start_pull_all(state.path.clone());
            server::json_response(serde_json::json!(s), 200)
        }),
        (Method::Get, ["pull-all", "status"]) => with_workspace(registry, &query, |state| match state.downloads.status("pull-all") {
            Some(s) => server::json_response(serde_json::json!(s), 200),
            None => server::json_response(serde_json::json!({ "error": "no pull-all job has been started" }), 404),
        }),
        (Method::Post, ["pull", node_id]) => with_workspace(registry, &query, |state| {
            let s = state.downloads.start_node(state.path.clone(), node_id.to_string());
            server::json_response(serde_json::json!(s), 200)
        }),
        (Method::Get, ["pull", node_id, "status"]) => with_workspace(registry, &query, |state| {
            match state.downloads.status(&format!("node:{node_id}")) {
                Some(s) => server::json_response(serde_json::json!(s), 200),
                None => server::json_response(serde_json::json!({ "error": format!("no download job for {node_id}") }), 404),
            }
        }),
        (Method::Get, ["runs"]) => {
            with_workspace(registry, &query, |state| server::json_response(serde_json::json!(state.runs.list()), 200))
        }
        (Method::Post, ["runs"]) => {
            let mut buf = String::new();
            let _ = request.as_reader().read_to_string(&mut buf);
            with_workspace(registry, &query, |state| {
                let spec: runs::RunSpec = if buf.trim().is_empty() {
                    runs::RunSpec { run_id: None, overrides: Default::default() }
                } else {
                    match serde_json::from_str(&buf) {
                        Ok(s) => s,
                        Err(e) => return server::json_response(serde_json::json!({ "error": e.to_string() }), 400),
                    }
                };
                match resolver::resolve_universe(&state.path).and_then(|g| state.runs.start(&g, spec)) {
                    Ok(s) => server::json_response(serde_json::json!(s), 200),
                    Err(e) => server::err_response(e),
                }
            })
        }
        (Method::Post, ["runs", run_id, "stop"]) => with_workspace(registry, &query, |state| match state.runs.stop(run_id) {
            Ok(()) => server::json_response(serde_json::json!({ "ok": true }), 200),
            Err(e) => server::err_response(e),
        }),
        (Method::Get, ["runs", run_id, "nodes", node_id, "logs"]) => with_workspace(registry, &query, |state| {
            let tail: usize = server::query_param(&query, "tail").and_then(|v| v.parse().ok()).unwrap_or(200);
            match state.runs.get(run_id) {
                Some(s) => match runs::logs_for(&s, node_id, tail) {
                    Ok(text) => server::json_response(serde_json::json!({ "logs": text }), 200),
                    Err(e) => server::err_response(e),
                },
                None => server::json_response(serde_json::json!({ "error": format!("no such run: {run_id}") }), 404),
            }
        }),
        _ => server::static_response(&route),
    };

    server::respond(request, response);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_id_is_deterministic_and_path_specific() {
        let a = workspace_id(Path::new("/tmp/fixtures/foo"));
        let b = workspace_id(Path::new("/tmp/fixtures/foo"));
        let c = workspace_id(Path::new("/tmp/fixtures/bar"));
        assert_eq!(a, b, "same path must hash to the same id");
        assert_ne!(a, c, "different paths must not collide");
        assert!(a.starts_with("foo-"), "id should carry a readable slug: {a}");
    }

    #[test]
    fn pid_alive_recognizes_self_and_init() {
        assert!(pid_alive(std::process::id()), "the current process must report as alive");
        // pid 1 (init/launchd) is root-owned; signaling it as a non-root
        // process exercises the EPERM-means-alive branch specifically.
        assert!(pid_alive(1), "pid 1 always exists and should count as alive via EPERM");
    }

    #[test]
    fn pid_alive_reports_missing_pid_as_dead() {
        assert!(!pid_alive(999_999_999), "an implausibly large pid should not exist");
    }

    #[test]
    fn resolve_is_idempotent_and_rejects_nested_workspaces() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = WorkspaceRegistry::load_from(tmp.path().join("workspaces.json"));

        let root = tmp.path().join("root");
        let (id1, canonical) = registry.resolve(None, Some(root.clone())).unwrap();

        // re-wiring the same path returns the same id, not a duplicate
        let (id2, _) = registry.resolve(None, Some(root.clone())).unwrap();
        assert_eq!(id1, id2);
        assert_eq!(registry.list().len(), 1);

        // the index on disk should reflect the single registered workspace
        let persisted = store::load_index(&tmp.path().join("workspaces.json"));
        assert_eq!(persisted.get(&id1), Some(&canonical));

        // registering a path inside an already-wired workspace must error
        let nested = root.join("nested-service");
        let err = registry.resolve(None, Some(nested)).unwrap_err();
        assert!(
            err.to_string().contains("is inside the already-wired workspace"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn stop_removes_workspace_from_registry_and_index() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = WorkspaceRegistry::load_from(tmp.path().join("workspaces.json"));
        let (id, _) = registry.resolve(None, Some(tmp.path().join("root"))).unwrap();

        assert!(registry.stop(&id));
        assert!(registry.get(&id).is_none());
        assert!(store::load_index(&tmp.path().join("workspaces.json")).get(&id).is_none());
        // stopping an unknown id is reported, not a panic
        assert!(!registry.stop(&id));
    }
}
