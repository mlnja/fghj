use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path as AxumPath, Query, State};
use axum::http::{header, request::Parts, StatusCode, Uri};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;

use crate::server::{self, WorkspaceState};
use crate::{ca, dns, docker, downloads, proxy, resolver, runs, store};

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

/// Where the control API's actual (ephemeral) port is published, mirroring
/// `pid_path` — only meaningful while `fghjd` is alive, so `/var/run` (not
/// the durable `/var/lib/fghjd` the CA lives under) is the right place.
pub fn port_path() -> PathBuf {
    PathBuf::from("/var/run/fghjd.port")
}

pub fn write_port(port: u16) -> Result<()> {
    std::fs::write(port_path(), port.to_string())
        .with_context(|| format!("failed to write port file {}", port_path().display()))
}

pub fn read_port() -> Option<u16> {
    std::fs::read_to_string(port_path()).ok()?.trim().parse().ok()
}

/// Durable storage for the local CA — must survive a reboot, unlike the
/// pidfile/port file, or every `fghjd` restart would need the user to
/// re-approve a brand new CA in Keychain Access.
fn ca_dir() -> PathBuf {
    PathBuf::from("/var/lib/fghjd/ca")
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

/// Looks up `key` in a `k=v&k=v` query string. No percent-decoding — the
/// only values passed through this today (workspace ids) never need it.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// In-memory registry of workspaces the daemon knows about, keyed by id.
/// Holds only data (path + per-workspace registries) — there is no thread or
/// listener tied to a workspace; all of them are served off the single axum
/// router, shared via `Router::with_state`.
pub struct WorkspaceRegistry {
    by_id: Mutex<HashMap<String, Arc<WorkspaceState>>>,
    index_path: PathBuf,
    docker: Arc<bollard::Docker>,
}

impl WorkspaceRegistry {
    /// Rebuilds the registry from the central workspace index at the real,
    /// root-owned path. `load_from` does the actual work — split out so
    /// tests can point the index at a tempdir instead.
    pub async fn load(docker: Arc<bollard::Docker>) -> Self {
        Self::load_from(store::default_index_path(), docker).await
    }

    async fn load_from(index_path: PathBuf, docker: Arc<bollard::Docker>) -> Self {
        let mut by_id = HashMap::new();
        for (id, path) in store::load_index(&index_path) {
            if !path.exists() {
                eprintln!("fghjd: skipping missing workspace {id} ({})", path.display());
                continue;
            }
            match WorkspaceState::new(path.clone(), docker.clone()).await {
                Ok(state) => {
                    by_id.insert(id, Arc::new(state));
                }
                Err(e) => eprintln!("fghjd: failed to load workspace {id} ({}): {e}", path.display()),
            }
        }
        Self { by_id: Mutex::new(by_id), index_path, docker }
    }

    /// Resolves (cloning `entry` if needed) and registers a workspace,
    /// reusing the existing entry if this path is already known. Errors if
    /// the path is nested inside an already-wired workspace — a workspace
    /// root covers its whole subtree, so a second registration underneath it
    /// would just be an alias for part of the same tree.
    pub async fn resolve(
        &self,
        entry: Option<String>,
        workspace: Option<PathBuf>,
        owner: Option<store::WorkspaceOwner>,
    ) -> Result<(String, PathBuf)> {
        let entry_for_meta = entry.clone();
        let owner_for_clone = owner.clone();
        let path = tokio::task::spawn_blocking(move || {
            crate::resolve_workspace(entry, workspace, owner_for_clone.as_ref())
        })
        .await
        .context("resolve_workspace task panicked")??;
        let canonical = std::fs::canonicalize(&path).unwrap_or(path);

        {
            let by_id = self.by_id.lock().unwrap();
            for state in by_id.values() {
                if canonical != state.path && canonical.starts_with(&state.path) {
                    bail!(
                        "{} is inside the already-wired workspace {}",
                        canonical.display(),
                        state.path.display()
                    );
                }
            }
        }

        let id = workspace_id(&canonical);
        let existing = self.by_id.lock().unwrap().get(&id).cloned();
        let state = match existing {
            Some(state) => state,
            None => {
                let state = Arc::new(WorkspaceState::new(canonical.clone(), self.docker.clone()).await?);
                state.db.clone().record_meta(id.clone(), entry_for_meta).await?;
                self.by_id.lock().unwrap().insert(id.clone(), state.clone());

                let mut index = store::load_index(&self.index_path);
                index.insert(id.clone(), canonical.clone());
                store::save_index(&self.index_path, &index)?;
                state
            }
        };

        // Refreshed on every `wire`, not just the first — the ssh-agent
        // socket captured here is only valid for the CLI's current login
        // session, so a later `wire` from a fresh session should replace it.
        if let Some(owner) = owner {
            state.db.clone().set_owner(id.clone(), owner).await?;
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
    pub async fn stop(&self, id: &str) -> bool {
        let removed = self.by_id.lock().unwrap().remove(id);
        match removed {
            Some(state) => {
                for run in state.runs.list() {
                    let _ = state.runs.stop(&run.run_id).await;
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
    #[serde(default)]
    owner: Option<store::WorkspaceOwner>,
}

#[derive(Deserialize)]
struct StopRequest {
    id: String,
}

fn err_response(e: anyhow::Error) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response()
}

fn bad_request(e: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e.to_string() }))).into_response()
}

/// Extracts the workspace named by `?workspace=<id>` in the request's query
/// string, or rejects with the same 400 the old handler used to return for a
/// missing/unknown id.
struct WorkspaceExtractor(Arc<WorkspaceState>);

impl FromRequestParts<Arc<WorkspaceRegistry>> for WorkspaceExtractor {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<WorkspaceRegistry>) -> Result<Self, Self::Rejection> {
        let query = parts.uri.query().unwrap_or("");
        match query_param(query, "workspace").and_then(|id| state.get(id)) {
            Some(ws) => Ok(WorkspaceExtractor(ws)),
            None => Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "unknown or missing ?workspace=<id>; POST /workspaces first" })),
            )
                .into_response()),
        }
    }
}

async fn get_workspaces(State(registry): State<Arc<WorkspaceRegistry>>) -> Response {
    let list: Vec<_> = registry
        .list()
        .into_iter()
        .map(|(id, workspace)| serde_json::json!({ "id": id, "workspace": workspace }))
        .collect();
    Json(serde_json::json!(list)).into_response()
}

async fn post_workspaces(State(registry): State<Arc<WorkspaceRegistry>>, body: Bytes) -> Response {
    match serde_json::from_slice::<StartRequest>(&body) {
        Ok(req) => match registry.resolve(req.entry, req.workspace, req.owner).await {
            Ok((id, workspace)) => Json(serde_json::json!({ "id": id, "workspace": workspace })).into_response(),
            Err(e) => err_response(e),
        },
        Err(e) => bad_request(e),
    }
}

async fn post_workspaces_stop(State(registry): State<Arc<WorkspaceRegistry>>, body: Bytes) -> Response {
    match serde_json::from_slice::<StopRequest>(&body) {
        Ok(req) => Json(serde_json::json!({ "stopped": registry.stop(&req.id).await })).into_response(),
        Err(e) => bad_request(e),
    }
}

async fn get_universe(WorkspaceExtractor(state): WorkspaceExtractor) -> Response {
    let path = state.path.clone();
    match tokio::task::spawn_blocking(move || resolver::resolve_universe(&path)).await {
        Ok(Ok(g)) => Json(serde_json::json!(g)).into_response(),
        Ok(Err(e)) => err_response(e),
        Err(e) => err_response(anyhow::anyhow!("resolve_universe task panicked: {e}")),
    }
}

#[derive(Deserialize)]
struct FlowQuery {
    flow: Option<String>,
}

async fn post_pull_all(Query(q): Query<FlowQuery>, WorkspaceExtractor(state): WorkspaceExtractor) -> Response {
    let owner = state.db.clone().load_owner().await.ok().flatten();
    let s = state.downloads.start_pull_all(state.path.clone(), owner, q.flow);
    Json(serde_json::json!(s)).into_response()
}

async fn get_pull_all_status(Query(q): Query<FlowQuery>, WorkspaceExtractor(state): WorkspaceExtractor) -> Response {
    let key = downloads::pull_all_key(q.flow.as_deref());
    match state.downloads.status(&key) {
        Some(s) => Json(serde_json::json!(s)).into_response(),
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "no pull-all job has been started" }))).into_response(),
    }
}

async fn post_pull_node(AxumPath(node_id): AxumPath<String>, WorkspaceExtractor(state): WorkspaceExtractor) -> Response {
    let owner = state.db.clone().load_owner().await.ok().flatten();
    let s = state.downloads.start_node(state.path.clone(), node_id, owner);
    Json(serde_json::json!(s)).into_response()
}

async fn get_pull_node_status(AxumPath(node_id): AxumPath<String>, WorkspaceExtractor(state): WorkspaceExtractor) -> Response {
    match state.downloads.status(&format!("node:{node_id}")) {
        Some(s) => Json(serde_json::json!(s)).into_response(),
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": format!("no download job for {node_id}") }))).into_response(),
    }
}

/// Lists every pull/download job the workspace has ever started, most-recent
/// first — backs the UI's single "operations queue" drawer.
async fn get_pull_jobs(WorkspaceExtractor(state): WorkspaceExtractor) -> Response {
    Json(serde_json::json!(state.downloads.list())).into_response()
}

async fn get_runs(WorkspaceExtractor(state): WorkspaceExtractor) -> Response {
    Json(serde_json::json!(state.runs.list())).into_response()
}

async fn post_runs(WorkspaceExtractor(state): WorkspaceExtractor, body: Bytes) -> Response {
    let spec: runs::RunSpec = if body.is_empty() {
        runs::RunSpec { run_id: None, overrides: Default::default(), flow: None }
    } else {
        match serde_json::from_slice(&body) {
            Ok(s) => s,
            Err(e) => return bad_request(e),
        }
    };

    let path = state.path.clone();
    let graph = match tokio::task::spawn_blocking(move || resolver::resolve_universe(&path)).await {
        Ok(Ok(g)) => g,
        Ok(Err(e)) => return err_response(e),
        Err(e) => return err_response(anyhow::anyhow!("resolve_universe task panicked: {e}")),
    };

    // A named run (review runs, with optional branch overrides) always
    // starts fresh under its own run_id. Anything else — "start default
    // environment" or a flow-scoped "run flow" click — targets the single
    // shared default environment and only tops up what isn't already
    // running, rather than tearing the whole thing down every click.
    let result = if spec.run_id.is_some() {
        state.runs.start(&graph, spec).await
    } else {
        state.runs.ensure_running(&graph, spec.flow.as_deref()).await
    };

    match result {
        Ok(s) => Json(serde_json::json!(s)).into_response(),
        Err(e) => err_response(e),
    }
}

async fn post_run_stop(AxumPath(run_id): AxumPath<String>, WorkspaceExtractor(state): WorkspaceExtractor) -> Response {
    match state.runs.stop(&run_id).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Deserialize)]
struct TailQuery {
    tail: Option<usize>,
}

async fn get_run_logs(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    Query(q): Query<TailQuery>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    let tail = q.tail.unwrap_or(200);
    match state.runs.get(&run_id) {
        Some(s) => match runs::logs_for_tail(&state.docker, &s, &node_id, tail).await {
            Ok(text) => Json(serde_json::json!({ "logs": text })).into_response(),
            Err(e) => err_response(e),
        },
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": format!("no such run: {run_id}") }))).into_response(),
    }
}

async fn get_run_logs_stream(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    let run_state = match state.runs.get(&run_id) {
        Some(s) => s,
        None => {
            return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": format!("no such run: {run_id}") }))).into_response()
        }
    };
    let container_name = match runs::container_name_for(&run_state, &node_id) {
        Ok(name) => name.to_string(),
        Err(e) => return bad_request(e),
    };

    let stream = docker::logs_follow(&state.docker, &container_name).map(|item| {
        let event = match item {
            Ok(chunk) => Event::default().data(chunk.to_string()),
            Err(e) => Event::default().event("error").data(e.to_string()),
        };
        Ok::<Event, Infallible>(event)
    });

    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

async fn static_handler(uri: Uri) -> Response {
    let (body, content_type, status) = server::static_response(uri.path());
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        [(header::CONTENT_TYPE, content_type)],
        body,
    )
        .into_response()
}

fn build_router(registry: Arc<WorkspaceRegistry>) -> Router {
    Router::new()
        .route("/workspaces", get(get_workspaces).post(post_workspaces))
        .route("/workspaces/stop", post(post_workspaces_stop))
        .route("/universe.json", get(get_universe))
        .route("/pull-all", post(post_pull_all))
        .route("/pull-all/status", get(get_pull_all_status))
        .route("/pull/{node_id}", post(post_pull_node))
        .route("/pull/{node_id}/status", get(get_pull_node_status))
        .route("/pull-jobs", get(get_pull_jobs))
        .route("/runs", get(get_runs).post(post_runs))
        .route("/runs/{run_id}/stop", post(post_run_stop))
        .route("/runs/{run_id}/nodes/{node_id}/logs", get(get_run_logs))
        .route("/runs/{run_id}/nodes/{node_id}/logs/stream", get(get_run_logs_stream))
        .fallback(static_handler)
        .with_state(registry)
}

/// Background loop, analogous to a Kubernetes controller's reconcile loop
/// but read-only: on each tick it re-inspects every workspace's live
/// containers and updates their recorded status (see `RunRegistry::refresh`)
/// so drift caused by someone `docker stop`/`rm`-ing a container by hand
/// shows up in the UI on its own, without a `fghjd` restart. It never
/// recreates or restarts anything — no self-healing, purely observational.
fn spawn_reconciler(registry: Arc<WorkspaceRegistry>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
        loop {
            interval.tick().await;
            for (id, _) in registry.list() {
                if let Some(state) = registry.get(&id) {
                    state.runs.refresh().await;
                }
            }
        }
    });
}

/// Connects to the Docker Engine API, preferring the plain `DOCKER_HOST`/
/// default-socket convention bollard understands natively, but falling back
/// to whatever socket the `docker` CLI's *active context* actually points
/// at. Docker Desktop, OrbStack, colima, etc. all route the `docker` command
/// through a context rather than the classic `/var/run/docker.sock` — a
/// concept bollard has no notion of — so without this fallback `fghjd` would
/// fail to connect on exactly the setups where `docker <cmd>` works fine.
fn connect_docker() -> Result<bollard::Docker> {
    match bollard::Docker::connect_with_local_defaults() {
        Ok(docker) => Ok(docker),
        Err(default_err) => {
            let context_host = Command::new("docker")
                .args(["context", "inspect", "--format", "{{.Endpoints.docker.Host}}"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|h| !h.is_empty());
            match context_host {
                Some(host) => bollard::Docker::connect_with_socket(&host, 120, bollard::API_DEFAULT_VERSION)
                    .with_context(|| format!("failed to connect to the docker context's socket ({host})")),
                None => Err(default_err).context("failed to construct a Docker client"),
            }
        }
    }
}

/// Connects to the Docker Engine API over its local socket, stands up DNS
/// (Subsystem B) and the TLS reverse proxy (Subsystem C) in front of the
/// control API, and serves the control API/UI forever. Fails fast if Docker
/// isn't reachable or any of the fixed/privileged ports (80, 443) or the
/// system trust store can't be bound/installed, rather than letting that
/// surface confusingly on the first request.
pub async fn run_control_api() -> Result<()> {
    let docker = connect_docker()?;
    docker
        .ping()
        .await
        .context("failed to reach the Docker daemon over its socket — is Docker running?")?;
    let docker = Arc::new(docker);

    // Bound (and OS-routed) before the control API comes up, so `fghjd`
    // fails fast on a bind error instead of silently serving the UI/API
    // without any *.fghj.internal resolution. The port is whatever the OS
    // handed out (see `dns::bind`), so `install_os_resolver_config` needs it
    // explicitly rather than assuming a fixed well-known one.
    let dns_socket = dns::bind().await?;
    let dns_port = dns_socket.local_addr().context("DNS socket has no local address")?.port();
    tokio::spawn(dns::serve(dns_socket));
    dns::install_os_resolver_config(dns_port)?;

    // The control API itself now binds an OS-assigned port too (see
    // `port_path`/`write_port`) — the CLI has no fixed port to hardcode
    // anymore, since it's fghj's own TLS proxy on 443 that owns the
    // well-known address (`https://fghj.internal`), not this listener.
    let control_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("failed to bind control API")?;
    let control_port = control_listener.local_addr().context("control API socket has no local address")?.port();
    write_port(control_port)?;

    let cert_path = ca::ca_cert_path(&ca_dir());
    let ca = {
        let dir = ca_dir();
        tokio::task::spawn_blocking(move || ca::ensure_ca(&dir)).await.context("CA setup task panicked")??
    };
    tokio::task::spawn_blocking(move || ca::install_macos_trust(&cert_path))
        .await
        .context("CA trust install task panicked")??;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert_resolver = Arc::new(ca::DynamicCertResolver::new(ca, provider.clone()));

    // Occupied before the registry loads and the control API is reachable
    // at all, so a bind failure on 80/443 ("something else is already
    // listening there") is reported clearly instead of leaving `fghjd`
    // half-started.
    let http_listener = proxy::bind_http().await?;
    let https_listener = proxy::bind_https().await?;
    tokio::spawn(proxy::serve_http_redirect(http_listener));
    tokio::spawn(proxy::serve_https(https_listener, cert_resolver, control_port, provider));

    let registry = Arc::new(WorkspaceRegistry::load(docker).await);
    spawn_reconciler(Arc::clone(&registry));

    let app = build_router(registry);
    println!("fghjd: control API listening on 127.0.0.1:{control_port} (reachable via https://{})", dns::ZONE);
    axum::serve(control_listener, app).await.context("control API server error")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_docker() -> Arc<bollard::Docker> {
        Arc::new(connect_docker().expect("docker client construction"))
    }

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

    #[tokio::test]
    async fn resolve_is_idempotent_and_rejects_nested_workspaces() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = WorkspaceRegistry::load_from(tmp.path().join("workspaces.json"), test_docker()).await;

        let root = tmp.path().join("root");
        let (id1, canonical) = registry.resolve(None, Some(root.clone()), None).await.unwrap();

        // re-wiring the same path returns the same id, not a duplicate
        let (id2, _) = registry.resolve(None, Some(root.clone()), None).await.unwrap();
        assert_eq!(id1, id2);
        assert_eq!(registry.list().len(), 1);

        // the index on disk should reflect the single registered workspace
        let persisted = store::load_index(&tmp.path().join("workspaces.json"));
        assert_eq!(persisted.get(&id1), Some(&canonical));

        // registering a path inside an already-wired workspace must error
        let nested = root.join("nested-service");
        let err = registry.resolve(None, Some(nested), None).await.unwrap_err();
        assert!(
            err.to_string().contains("is inside the already-wired workspace"),
            "unexpected error message: {err}"
        );
    }

    #[tokio::test]
    async fn stop_removes_workspace_from_registry_and_index() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = WorkspaceRegistry::load_from(tmp.path().join("workspaces.json"), test_docker()).await;
        let (id, _) = registry.resolve(None, Some(tmp.path().join("root")), None).await.unwrap();

        assert!(registry.stop(&id).await);
        assert!(registry.get(&id).is_none());
        assert!(store::load_index(&tmp.path().join("workspaces.json")).get(&id).is_none());
        // stopping an unknown id is reported, not a panic
        assert!(!registry.stop(&id).await);
    }
}
