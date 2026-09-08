use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path as AxumPath, Query, State};
use axum::http::{StatusCode, Uri, header, request::Parts};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::server::{self, WorkspaceState};
use crate::{ca, dns, docker, downloads, hosts_file, proxy, resolver, runs, store};

/// How often the background reconciler re-inspects live containers. Kept in
/// step with the frontend's `/runs` poll interval (see `App.svelte`) so the
/// UI is essentially never stale.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);

/// Where `fghjd`'s control API listens — a Unix socket rather than a TCP
/// port, dockerd-style: it's local-machine-only by nature (no port to pick,
/// collide with, or scan) and access control is a filesystem permission
/// (see `run_control_api`'s `chmod` after bind) instead of "trust anything
/// that can reach 127.0.0.1". Only meaningful while `fghjd` is alive, so
/// `/var/run` (not the durable `/var/lib/fghjd` the CA lives under) is the
/// right place.
pub fn socket_path() -> PathBuf {
    PathBuf::from("/var/run/fghjd.sock")
}

/// Durable storage for the local CA — must survive a reboot, or every
/// `fghjd` restart would need the user to re-approve a brand new CA in
/// Keychain Access.
fn ca_dir() -> PathBuf {
    PathBuf::from("/var/lib/fghjd/ca")
}

/// Tracks that the operator's last explicit `fghj daemon` call was `stop`,
/// not just that `fghjd` currently happens to be idle in memory. Backed by
/// `store::DaemonState` at `store::default_state_path()` — next to the CA
/// (durable, survives a reboot) rather than under `/var/run`: "I told it to
/// stop" is a standing instruction that should hold until countermanded by
/// `fghj daemon start`, not something a crash or a reboot should silently
/// discard by reactivating anyway. `idle_requested` is read-modify-write
/// against the whole state file, same as every other field it may grow.
fn is_idle_requested() -> bool {
    store::load_daemon_state(&store::default_state_path()).idle_requested
}

fn set_idle_requested(idle_requested: bool) -> Result<()> {
    let path = store::default_state_path();
    let mut state = store::load_daemon_state(&path);
    state.idle_requested = idle_requested;
    store::save_daemon_state(&path, &state)
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
    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");
    let slug: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
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
                eprintln!(
                    "fghjd: skipping missing workspace {id} ({})",
                    path.display()
                );
                continue;
            }
            match WorkspaceState::new(path.clone(), docker.clone()).await {
                Ok(state) => {
                    by_id.insert(id, Arc::new(state));
                }
                Err(e) => eprintln!(
                    "fghjd: failed to load workspace {id} ({}): {e}",
                    path.display()
                ),
            }
        }
        Self {
            by_id: Mutex::new(by_id),
            index_path,
            docker,
        }
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
                let state =
                    Arc::new(WorkspaceState::new(canonical.clone(), self.docker.clone()).await?);
                state
                    .db
                    .clone()
                    .record_meta(id.clone(), entry_for_meta)
                    .await?;
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

    /// Finds the `127.0.0.1:<port>` a running container in any wired
    /// workspace's active runs publishes `host` at, if any — the SNI ->
    /// container lookup backing real per-service HTTPS routing (see
    /// `proxy::serve_https`). Only `"running"` containers are considered, so
    /// a stopped-but-not-yet-reconciled container's stale route doesn't hand
    /// back a dead port.
    pub fn resolve_route(&self, host: &str) -> Option<u16> {
        let states: Vec<Arc<WorkspaceState>> =
            self.by_id.lock().unwrap().values().cloned().collect();
        // Exact matches (a node's own derived domain, a named port, or a
        // literal `#AdditionalHost`) always win over a wildcard match — a
        // `wildcard_hosts` suffix only ever fills in for a name nothing more
        // specific already claims.
        states
            .iter()
            .find_map(|state| {
                state.runs.list().iter().find_map(|run| {
                    run.containers
                        .iter()
                        .filter(|c| c.status == "running")
                        .find_map(|c| {
                            c.routes
                                .iter()
                                .find(|r| !r.wildcard && r.domain == host)
                                .map(|r| r.host_port)
                        })
                })
            })
            .or_else(|| {
                states.iter().find_map(|state| {
                    state.runs.list().iter().find_map(|run| {
                        run.containers
                            .iter()
                            .filter(|c| c.status == "running")
                            .find_map(|c| {
                                c.routes
                                    .iter()
                                    .find(|r| {
                                        r.wildcard
                                            && (host == r.domain
                                                || host.ends_with(&format!(".{}", r.domain)))
                                    })
                                    .map(|r| r.host_port)
                            })
                    })
                })
            })
    }

    /// Every `#AdditionalHost` alias currently claimed by a `"running"`
    /// container in any wired workspace, sorted and deduplicated — the input
    /// to `hosts_file::sync`. Recomputed from scratch on every call (mirrors
    /// `resolve_route`'s own linear scan) rather than tracked incrementally,
    /// since it's only ever called once per reconciler tick.
    pub fn active_additional_hosts(&self) -> Vec<String> {
        let states: Vec<Arc<WorkspaceState>> =
            self.by_id.lock().unwrap().values().cloned().collect();
        let mut hosts: Vec<String> = states
            .iter()
            .flat_map(|state| {
                state.runs.list().into_iter().flat_map(|run| {
                    run.containers
                        .into_iter()
                        .filter(|c| c.status == "running")
                        .flat_map(|c| c.additional_hosts)
                })
            })
            .collect();
        hosts.sort();
        hosts.dedup();
        hosts
    }

    /// Every `wildcard_hosts` suffix currently claimed by a `"running"`
    /// container in any wired workspace, sorted and deduplicated — the input
    /// to `dns::install_os_resolver_config`'s per-zone `/etc/resolver` sync.
    /// Same recompute-from-scratch approach as `active_additional_hosts`.
    pub fn active_wildcard_suffixes(&self) -> Vec<String> {
        let states: Vec<Arc<WorkspaceState>> =
            self.by_id.lock().unwrap().values().cloned().collect();
        let mut zones: Vec<String> = states
            .iter()
            .flat_map(|state| {
                state.runs.list().into_iter().flat_map(|run| {
                    run.containers
                        .into_iter()
                        .filter(|c| c.status == "running")
                        .flat_map(|c| {
                            c.routes
                                .into_iter()
                                .filter(|r| r.wildcard)
                                .map(|r| r.domain)
                        })
                })
            })
            .collect();
        zones.sort();
        zones.dedup();
        zones
    }

    pub fn list(&self) -> Vec<(String, PathBuf)> {
        self.by_id
            .lock()
            .unwrap()
            .iter()
            .map(|(id, s)| (id.clone(), s.path.clone()))
            .collect()
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

impl proxy::RouteResolver for WorkspaceRegistry {
    fn resolve(&self, host: &str) -> Option<u16> {
        self.resolve_route(host)
    }
}

impl dns::ZoneSource for WorkspaceRegistry {
    fn active_wildcard_zones(&self) -> Vec<String> {
        self.active_wildcard_suffixes()
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
    // `e.to_string()` (anyhow's `Display`) only prints the outermost
    // `.context(...)` layer — e.g. just "docker run <name> failed" with the
    // actual Docker Engine API error message it wraps discarded. `{e:?}`
    // (anyhow's `Debug`) prints the full "Caused by:" chain instead, which is
    // the only version that's actually diagnosable from the API response.
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": format!("{e:?}") })),
    )
        .into_response()
}

fn bad_request(e: impl std::fmt::Display) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": e.to_string() })),
    )
        .into_response()
}

/// Extracts the workspace named by `?workspace=<id>` in the request's query
/// string, or rejects with the same 400 the old handler used to return for a
/// missing/unknown id.
struct WorkspaceExtractor(Arc<WorkspaceState>);

impl FromRequestParts<Arc<WorkspaceRegistry>> for WorkspaceExtractor {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<WorkspaceRegistry>,
    ) -> Result<Self, Self::Rejection> {
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
            Ok((id, workspace)) => {
                Json(serde_json::json!({ "id": id, "workspace": workspace })).into_response()
            }
            Err(e) => err_response(e),
        },
        Err(e) => bad_request(e),
    }
}

async fn post_workspaces_stop(
    State(registry): State<Arc<WorkspaceRegistry>>,
    body: Bytes,
) -> Response {
    match serde_json::from_slice::<StopRequest>(&body) {
        Ok(req) => {
            Json(serde_json::json!({ "stopped": registry.stop(&req.id).await })).into_response()
        }
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

async fn post_pull_all(
    Query(q): Query<FlowQuery>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    let owner = state.db.clone().load_owner().await.ok().flatten();
    let s = state
        .downloads
        .start_pull_all(state.path.clone(), owner, q.flow);
    Json(serde_json::json!(s)).into_response()
}

async fn get_pull_all_status(
    Query(q): Query<FlowQuery>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    let key = downloads::pull_all_key(q.flow.as_deref());
    match state.downloads.status(&key) {
        Some(s) => Json(serde_json::json!(s)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no pull-all job has been started" })),
        )
            .into_response(),
    }
}

async fn post_pull_node(
    AxumPath(node_id): AxumPath<String>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    let owner = state.db.clone().load_owner().await.ok().flatten();
    let s = state
        .downloads
        .start_node(state.path.clone(), node_id, owner);
    Json(serde_json::json!(s)).into_response()
}

async fn get_pull_node_status(
    AxumPath(node_id): AxumPath<String>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state.downloads.status(&format!("node:{node_id}")) {
        Some(s) => Json(serde_json::json!(s)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no download job for {node_id}") })),
        )
            .into_response(),
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
        runs::RunSpec {
            run_id: None,
            overrides: Default::default(),
            flow: None,
        }
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
        state
            .runs
            .ensure_running(&graph, spec.flow.as_deref())
            .await
    };

    match result {
        Ok(s) => Json(serde_json::json!(s)).into_response(),
        Err(e) => err_response(e),
    }
}

async fn post_run_stop(
    AxumPath(run_id): AxumPath<String>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state.runs.stop(&run_id).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn post_run_node_start(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    let path = state.path.clone();
    let graph = match tokio::task::spawn_blocking(move || resolver::resolve_universe(&path)).await {
        Ok(Ok(g)) => g,
        Ok(Err(e)) => return err_response(e),
        Err(e) => return err_response(anyhow::anyhow!("resolve_universe task panicked: {e}")),
    };
    match state
        .runs
        .restart_container(&graph, &run_id, &node_id)
        .await
    {
        Ok(info) => Json(serde_json::json!(info)).into_response(),
        Err(e) => err_response(e),
    }
}

async fn post_run_node_stop(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state.runs.stop_container(&run_id, &node_id).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn post_run_node_delete(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state.runs.remove_container(&run_id, &node_id).await {
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
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no such run: {run_id}") })),
        )
            .into_response(),
    }
}

async fn get_run_logs_stream(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    let run_state = match state.runs.get(&run_id) {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("no such run: {run_id}") })),
            )
                .into_response();
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

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// First message a client must send once the socket is upgraded — everything
/// `docker::exec_start` needs. `cols`/`rows` are only meaningful when `tty`.
#[derive(Deserialize)]
struct ExecStart {
    cmd: Vec<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    tty: bool,
    #[serde(default)]
    cols: u16,
    #[serde(default)]
    rows: u16,
}

/// In-band control messages a client can send after the initial `ExecStart`
/// — sent as `Message::Text` (JSON), distinct from `Message::Binary`, which
/// is always raw stdin bytes for the exec'd process.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ExecControl {
    Resize { cols: u16, rows: u16 },
}

/// `GET /runs/{run_id}/nodes/{node_id}/exec/ws?workspace={id}` — proxies a
/// real `docker exec` full duplex: see `docker::ExecSession`'s doc comment.
/// Resolves the run/node the same way `get_run_logs_stream` does, as a plain
/// pre-upgrade HTTP response, so a bad run/node id gets a clean 404/400
/// instead of failing mid-handshake.
async fn get_run_exec_ws(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    WorkspaceExtractor(state): WorkspaceExtractor,
    ws: WebSocketUpgrade,
) -> Response {
    let run_state = match state.runs.get(&run_id) {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("no such run: {run_id}") })),
            )
                .into_response();
        }
    };
    let container_name = match runs::container_name_for(&run_state, &node_id) {
        Ok(name) => name.to_string(),
        Err(e) => return bad_request(e),
    };

    let docker = state.docker.clone();
    ws.on_upgrade(move |socket| handle_exec_socket(socket, docker, container_name))
}

/// Drives one exec session end to end: reads the initial `ExecStart`, starts
/// the exec, then relays bytes both directions until the exec's output ends,
/// finally reporting its exit code. Any failure along the way (bad start
/// message, exec creation failure, a dropped socket) just ends the task —
/// there's no client left to usefully report to once the duplex channel
/// itself is the thing that broke.
async fn handle_exec_socket(
    mut socket: WebSocket,
    docker: Arc<bollard::Docker>,
    container_name: String,
) {
    let start = match socket.recv().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<ExecStart>(&text) {
            Ok(start) => start,
            Err(e) => {
                let _ = socket
                    .send(exec_error_message(&format!(
                        "invalid exec start message: {e}"
                    )))
                    .await;
                return;
            }
        },
        _ => return,
    };

    let mut session = match docker::exec_start(
        &docker,
        &container_name,
        &start.cmd,
        start.user.as_deref(),
        start.working_dir.as_deref(),
        start.tty,
    )
    .await
    {
        Ok(session) => session,
        Err(e) => {
            let _ = socket.send(exec_error_message(&e.to_string())).await;
            return;
        }
    };

    if start.tty {
        let _ =
            docker::exec_resize(&docker, &session.id, start.cols.max(1), start.rows.max(1)).await;
    }

    loop {
        tokio::select! {
            item = session.output.next() => {
                match item {
                    Some(Ok(chunk)) => {
                        if socket.send(Message::Binary(chunk.into_bytes())).await.is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Binary(bytes))) if bytes.is_empty() => {
                        // Local stdin hit EOF (e.g. Ctrl-D, or a piped
                        // command's input ran out) — shut down the exec's
                        // stdin without tearing down the whole socket, so a
                        // command reading until EOF can finish and still
                        // stream its remaining output back.
                        let _ = session.input.shutdown().await;
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if session.input.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(ExecControl::Resize { cols, rows }) = serde_json::from_str(&text) {
                            let _ = docker::exec_resize(&docker, &session.id, cols, rows).await;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        }
    }

    let code = docker::exec_exit_code(&docker, &session.id)
        .await
        .unwrap_or(-1);
    let _ = socket
        .send(Message::Text(
            serde_json::json!({ "type": "exit", "code": code })
                .to_string()
                .into(),
        ))
        .await;
}

fn exec_error_message(message: &str) -> Message {
    Message::Text(
        serde_json::json!({ "type": "error", "message": message })
            .to_string()
            .into(),
    )
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

fn build_router(registry: Arc<WorkspaceRegistry>, daemon: Arc<DaemonControl>) -> Router {
    let api = Router::new()
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
        .route(
            "/runs/{run_id}/nodes/{node_id}/start",
            post(post_run_node_start),
        )
        .route(
            "/runs/{run_id}/nodes/{node_id}/stop",
            post(post_run_node_stop),
        )
        .route(
            "/runs/{run_id}/nodes/{node_id}/delete",
            post(post_run_node_delete),
        )
        .route("/runs/{run_id}/nodes/{node_id}/logs", get(get_run_logs))
        .route(
            "/runs/{run_id}/nodes/{node_id}/logs/stream",
            get(get_run_logs_stream),
        )
        .route(
            "/runs/{run_id}/nodes/{node_id}/exec/ws",
            get(get_run_exec_ws),
        )
        .with_state(registry);

    let daemon_api = Router::new()
        .route("/daemon/start", post(post_daemon_start))
        .route("/daemon/stop", post(post_daemon_stop))
        .route("/daemon/status", get(get_daemon_status))
        .with_state(daemon);

    api.merge(daemon_api).fallback(static_handler)
}

/// `fghj daemon start` — reconciles `fghjd` back into the active state
/// (rebinds DNS/80/443, resyncs `/etc/hosts`). Idempotent: calling it while
/// already active just reports the current state back. Clears the
/// `idle_requested` flag in `store::DaemonState` on success so a later
/// crash/reboot restart comes back active too, instead of silently
/// reverting to idle.
async fn post_daemon_start(State(daemon): State<Arc<DaemonControl>>) -> Response {
    match daemon.activate().await {
        Ok(()) => {
            if let Err(e) = set_idle_requested(false) {
                eprintln!("fghjd: failed to persist daemon state: {e}");
            }
            Json(serde_json::json!({ "active": true })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `fghj daemon stop` — releases 80/443/DNS and clears `/etc/hosts` without
/// touching the `fghjd` process itself (it keeps serving this control API so
/// a later `fghj daemon start` can reach it). Docker containers already
/// running are left alone. Also persists `idle_requested` in
/// `store::DaemonState` so a crash or reboot before the next `start` doesn't
/// silently reactivate `fghjd` against the operator's wishes.
async fn post_daemon_stop(State(daemon): State<Arc<DaemonControl>>) -> Response {
    daemon.deactivate();
    if let Err(e) = set_idle_requested(true) {
        eprintln!("fghjd: failed to persist daemon state: {e}");
    }
    Json(serde_json::json!({ "active": false })).into_response()
}

async fn get_daemon_status(State(daemon): State<Arc<DaemonControl>>) -> Response {
    Json(serde_json::json!({ "active": daemon.is_active() })).into_response()
}

/// The pieces of `fghjd` that only exist while it's in the "active" state:
/// the DNS server, and the HTTP/HTTPS reverse proxy occupying 80/443.
/// Dropping (aborting) these tasks frees the ports/socket they held.
struct ActiveResources {
    dns_task: tokio::task::JoinHandle<()>,
    http_task: tokio::task::JoinHandle<()>,
    https_task: tokio::task::JoinHandle<()>,
    /// The port fghjd's DNS server bound to, kept around so the reconciler
    /// can re-sync `/etc/resolver` files (one per active `wildcard_hosts`
    /// zone) on every tick without re-deriving it.
    dns_port: u16,
}

/// `fghjd` itself is meant to run forever — started at boot and restarted on
/// crash by the OS service manager (launchd/systemd) — but the user still
/// needs a way to tell it to get out of the way without stopping the whole
/// process: release ports 80/443, stop answering `*.fghj.internal` DNS
/// queries, and drop every `#AdditionalHost` entry from `/etc/hosts`, while
/// staying alive and reachable so a later `fghj daemon start` can reconcile
/// everything back. `DaemonControl` is that on/off switch: the control API
/// (always up) holds one of these and toggles `active` in response to
/// `/daemon/start` and `/daemon/stop`.
pub struct DaemonControl {
    registry: Arc<WorkspaceRegistry>,
    cert_resolver: Arc<ca::DynamicCertResolver>,
    provider: Arc<rustls::crypto::CryptoProvider>,
    control_port: u16,
    active: Mutex<Option<ActiveResources>>,
}

impl DaemonControl {
    pub fn is_active(&self) -> bool {
        self.active.lock().unwrap().is_some()
    }

    /// Binds the DNS server and the HTTP/HTTPS proxy, installs the OS
    /// resolver config, and syncs `/etc/hosts` — i.e. makes `fghjd` actually
    /// reachable at `*.fghj.internal` (and any declared additional hosts).
    /// A no-op if already active, so it's safe to call from `fghj daemon
    /// start` unconditionally without checking status first.
    pub async fn activate(&self) -> Result<()> {
        if self.is_active() {
            return Ok(());
        }

        let dns_socket = dns::bind().await?;
        let dns_port = dns_socket
            .local_addr()
            .context("DNS socket has no local address")?
            .port();
        let dns_task = tokio::spawn(dns::serve(dns_socket, self.registry.clone()));
        dns::install_os_resolver_config(dns_port, &self.registry.active_wildcard_suffixes())?;

        let http_listener = proxy::bind_http().await?;
        let https_listener = proxy::bind_https().await?;
        let http_task = tokio::spawn(proxy::serve_http_redirect(
            http_listener,
            self.registry.clone(),
        ));
        let https_task = tokio::spawn(proxy::serve_https(
            https_listener,
            self.cert_resolver.clone(),
            self.control_port,
            self.provider.clone(),
            self.registry.clone(),
        ));

        hosts_file::sync(
            &hosts_file::hosts_path(),
            &self.registry.active_additional_hosts(),
        )?;

        *self.active.lock().unwrap() = Some(ActiveResources {
            dns_task,
            http_task,
            https_task,
            dns_port,
        });
        Ok(())
    }

    /// The port fghjd's DNS server is currently bound to, if active — used
    /// by `spawn_reconciler` to re-sync `/etc/resolver` files without
    /// needing to re-derive or re-bind anything. `None` while idle.
    fn active_dns_port(&self) -> Option<u16> {
        self.active.lock().unwrap().as_ref().map(|r| r.dns_port)
    }

    /// Reverses `activate`: aborts the DNS/HTTP/HTTPS tasks (freeing the
    /// ports/socket they held) and clears fghj's managed entries from the OS
    /// resolver config and `/etc/hosts`. Docker containers already running
    /// are untouched — they keep running under Docker's own supervision and
    /// are simply unreachable until the next `activate`. A no-op if already
    /// idle.
    pub fn deactivate(&self) {
        if let Some(resources) = self.active.lock().unwrap().take() {
            resources.dns_task.abort();
            resources.http_task.abort();
            resources.https_task.abort();
        }
        dns::clear_os_resolver_config();
        if let Err(e) = hosts_file::sync(&hosts_file::hosts_path(), &[]) {
            eprintln!("fghjd: failed to clear /etc/hosts on deactivate: {e}");
        }
    }
}

/// Background loop, analogous to a Kubernetes controller's reconcile loop
/// but read-only with respect to Docker: on each tick it re-inspects every
/// workspace's live containers and updates their recorded status (see
/// `RunRegistry::refresh`) so drift caused by someone `docker stop`/`rm`-ing
/// a container by hand shows up in the UI on its own, without a `fghjd`
/// restart. It never recreates or restarts a container — no self-healing
/// there. It does own one side effect outside Docker, though: re-syncing
/// `/etc/hosts` (`hosts_file::sync`) to exactly the `#AdditionalHost`
/// aliases of whatever's currently `"running"`, so a container dying
/// out-of-band (same drift this loop already detects) also drops its alias
/// within one tick, not just its status. Skipped entirely while `fghjd` is
/// idle (`daemon.is_active()` is false) so it doesn't fight `fghj daemon
/// stop`'s clean-up by re-adding entries `deactivate` just removed.
fn spawn_reconciler(daemon: Arc<DaemonControl>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
        loop {
            interval.tick().await;
            for (id, _) in daemon.registry.list() {
                if let Some(state) = daemon.registry.get(&id) {
                    state.runs.refresh().await;
                }
            }
            if !daemon.is_active() {
                continue;
            }
            if let Err(e) = hosts_file::sync(
                &hosts_file::hosts_path(),
                &daemon.registry.active_additional_hosts(),
            ) {
                eprintln!("fghjd: failed to sync /etc/hosts: {e}");
            }
            if let Some(dns_port) = daemon.active_dns_port()
                && let Err(e) = dns::install_os_resolver_config(
                    dns_port,
                    &daemon.registry.active_wildcard_suffixes(),
                )
            {
                eprintln!("fghjd: failed to sync /etc/resolver: {e}");
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
pub(crate) fn connect_docker() -> Result<bollard::Docker> {
    match bollard::Docker::connect_with_local_defaults() {
        Ok(docker) => Ok(docker),
        Err(default_err) => {
            let context_host = Command::new("docker")
                .args([
                    "context",
                    "inspect",
                    "--format",
                    "{{.Endpoints.docker.Host}}",
                ])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|h| !h.is_empty());
            match context_host {
                Some(host) => {
                    bollard::Docker::connect_with_socket(&host, 120, bollard::API_DEFAULT_VERSION)
                        .with_context(|| {
                            format!("failed to connect to the docker context's socket ({host})")
                        })
                }
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
///
/// `fghjd` itself never exits on its own after this point except via a
/// terminating signal (SIGTERM/SIGINT) — the intent is that it's started at
/// boot and supervised (launchd/systemd), restarting on crash, for the life
/// of the machine. `fghj daemon stop`/`start` (see `DaemonControl`) toggle
/// whether it's actively occupying ports/DNS/`/etc/hosts` without touching
/// this process's lifecycle at all; a real signal is reserved for an actual
/// shutdown (service uninstall/restart, system shutdown), at which point we
/// still deactivate first so we don't leave stale ports/hosts entries behind
/// for whatever comes next.
pub async fn run_control_api() -> Result<()> {
    let docker = connect_docker()?;
    docker
        .ping()
        .await
        .context("failed to reach the Docker daemon over its socket — is Docker running?")?;
    let docker = Arc::new(docker);

    // The control API's OS-assigned TCP port is what the HTTPS proxy relays
    // `https://fghj.internal` to internally (see `proxy::serve_https`'s
    // `control_port` param) — never dialed directly by anything else, so it
    // doesn't need to be fixed or discoverable. Bound before anything else so
    // there's something for the proxy to relay to even if activation below
    // fails partway.
    let control_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("failed to bind control API")?;
    let control_port = control_listener
        .local_addr()
        .context("control API socket has no local address")?
        .port();

    // The `fghj` CLI talks to the same control API over a Unix socket
    // instead — dockerd-style: no port to pick or discover, and access is a
    // filesystem permission rather than "anything that can reach
    // 127.0.0.1". `fghjd` runs as root while `fghj` runs as the invoking
    // user, so the socket needs opening up beyond its default root-only
    // permissions for the CLI to reach it at all.
    let socket_path = socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let cli_listener = tokio::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind control socket {}", socket_path.display()))?;
    std::fs::set_permissions(
        &socket_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o666),
    )
    .with_context(|| format!("failed to set permissions on {}", socket_path.display()))?;

    let cert_path = ca::ca_cert_path(&ca_dir());
    let ca = {
        let dir = ca_dir();
        tokio::task::spawn_blocking(move || ca::ensure_ca(&dir))
            .await
            .context("CA setup task panicked")??
    };
    tokio::task::spawn_blocking(move || ca::install_macos_trust(&cert_path))
        .await
        .context("CA trust install task panicked")??;

    let provider = Arc::new(rustls::crypto::ring::default_provider());

    // Loaded before the cert resolver: the cert resolver needs it too, to
    // gate certificate issuance for a reserved-TLD `#AdditionalHost` alias on
    // "is some running container actually claiming this name as a route"
    // (see `ca::DynamicCertResolver::resolve_for`), not just "is this in our
    // own zone".
    let registry = Arc::new(WorkspaceRegistry::load(docker).await);

    let cert_resolver = Arc::new(ca::DynamicCertResolver::new(
        ca,
        provider.clone(),
        registry.clone(),
    ));

    let daemon = Arc::new(DaemonControl {
        registry: registry.clone(),
        cert_resolver,
        provider,
        control_port,
        active: Mutex::new(None),
    });
    spawn_reconciler(Arc::clone(&daemon));

    // `fghjd` starts active by default: it's meant to occupy 80/443 and
    // *.fghj.internal DNS from the moment the system boots. The one
    // exception is `is_idle_requested()` — if the operator's last explicit
    // `fghj daemon` call was `stop`, a crash or reboot in between must not
    // silently override that by reactivating anyway; staying idle here is
    // what makes `fghj daemon stop` a durable instruction rather than a
    // one-shot action that a flaky Docker daemon or a reboot can undo behind
    // the operator's back. A bind failure during activation (e.g.
    // "something else is already listening on 80/443") still fails startup
    // fast, before the control API ever serves a request.
    if is_idle_requested() {
        println!(
            "fghjd: starting idle — last `fghj daemon` action was `stop`; run `fghj daemon start` to reconcile"
        );
    } else {
        daemon.activate().await?;
    }

    let app = build_router(registry, Arc::clone(&daemon));
    println!(
        "fghjd: control API listening on {} (CLI) and reachable via https://{}",
        socket_path.display(),
        dns::ZONE
    );

    // Same router, two listeners: the Unix socket is the CLI's channel, the
    // TCP one is only ever dialed internally by the HTTPS proxy's apex-name
    // relay (see above) — `Router` is cheap to clone (an `Arc` internally).
    let cli_app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(cli_listener, cli_app).await {
            eprintln!("fghjd: control socket server error: {e}");
        }
    });

    let shutdown_signal = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("failed to install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
    };

    tokio::select! {
        result = axum::serve(control_listener, app) => {
            result.context("control API server error")?;
        }
        _ = shutdown_signal => {
            println!("fghjd: received shutdown signal, releasing ports and cleaning up...");
            daemon.deactivate();
        }
    }

    let _ = std::fs::remove_file(&socket_path);
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
        assert!(
            a.starts_with("foo-"),
            "id should carry a readable slug: {a}"
        );
    }

    #[tokio::test]
    async fn resolve_is_idempotent_and_rejects_nested_workspaces() {
        let tmp = tempfile::tempdir().unwrap();
        let registry =
            WorkspaceRegistry::load_from(tmp.path().join("workspaces.json"), test_docker()).await;

        let root = tmp.path().join("root");
        let (id1, canonical) = registry
            .resolve(None, Some(root.clone()), None)
            .await
            .unwrap();

        // re-wiring the same path returns the same id, not a duplicate
        let (id2, _) = registry
            .resolve(None, Some(root.clone()), None)
            .await
            .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(registry.list().len(), 1);

        // the index on disk should reflect the single registered workspace
        let persisted = store::load_index(&tmp.path().join("workspaces.json"));
        assert_eq!(persisted.get(&id1), Some(&canonical));

        // registering a path inside an already-wired workspace must error
        let nested = root.join("nested-service");
        let err = registry
            .resolve(None, Some(nested), None)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("is inside the already-wired workspace"),
            "unexpected error message: {err}"
        );
    }

    #[tokio::test]
    async fn stop_removes_workspace_from_registry_and_index() {
        let tmp = tempfile::tempdir().unwrap();
        let registry =
            WorkspaceRegistry::load_from(tmp.path().join("workspaces.json"), test_docker()).await;
        let (id, _) = registry
            .resolve(None, Some(tmp.path().join("root")), None)
            .await
            .unwrap();

        assert!(registry.stop(&id).await);
        assert!(registry.get(&id).is_none());
        assert!(!store::load_index(&tmp.path().join("workspaces.json")).contains_key(&id));
        // stopping an unknown id is reported, not a panic
        assert!(!registry.stop(&id).await);
    }
}
