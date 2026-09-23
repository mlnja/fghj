use std::collections::HashMap;
use std::convert::Infallible;
use std::net::Ipv4Addr;
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
use crate::{
    action, actor, ca, daemon_log, dns, docker, downloads, effects, hosts_file, persistence, proxy,
    raw_net, registry, resolver, run_view, runs, state,
};

/// How often the background reconciler re-inspects live containers. Kept in
/// step with the frontend's `/runs` poll interval (see `App.svelte`) so the
/// UI is essentially never stale.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);

/// How often `spawn_sync_reconciler` re-resolves each workspace's
/// `.fghj.yaml` and recomputes config-drift hashes. Deliberately much
/// coarser than `RECONCILE_INTERVAL`: unlike `refresh` (a handful of Docker
/// inspect calls), this re-runs full CUE resolution — parsing every
/// `.fghj.yaml` in the workspace from scratch — which is real work not worth
/// repeating every second just to catch drift that, by definition, only
/// happens when someone edits a config file by hand.
const SYNC_RECONCILE_INTERVAL: Duration = Duration::from_secs(15);

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
/// Keychain Access. `pub(crate)` so `runs.rs` can bind-mount it (read-only)
/// into a run's sidecar proxy container.
pub(crate) fn ca_dir() -> PathBuf {
    persistence::fghjd_root().join("ca")
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
    /// New-system actor for every workspace this registry knows about — the
    /// redux-style migration's (rosy-soaring-teapot.md) canonical
    /// `state::WorkspaceState`, authored entirely by the reducer and
    /// converged to real Docker/DNS/hosts/raw-net state by the effects in
    /// `docker_converge_tasks` and `effects::spawn_all`. Kept alongside
    /// `by_id` rather than merged into it: `by_id`'s old `server::WorkspaceState`
    /// still owns the real Docker orchestration (`RunRegistry`) this phase
    /// reuses, persistence, and the live routing/DNS lookup paths that
    /// haven't migrated yet.
    actors: registry::ActorRegistry,
    /// Per-workspace `effects::docker::DockerConvergeEffect` driver tasks —
    /// the only thing that ever mutates a workspace's real containers now
    /// that `effects::bridge` (a purely-mirroring, never-mutating stand-in)
    /// is gone. Torn down in `stop`.
    docker_converge_tasks: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl WorkspaceRegistry {
    /// Rebuilds the registry from the central workspace index at the real,
    /// root-owned path. `load_from` does the actual work — split out so
    /// tests can point the index at a tempdir instead.
    pub async fn load(docker: Arc<bollard::Docker>) -> Self {
        Self::load_from(persistence::default_index_path(), docker).await
    }

    async fn load_from(index_path: PathBuf, docker: Arc<bollard::Docker>) -> Self {
        let mut by_id = HashMap::new();
        for (id, path) in persistence::load_index(&index_path) {
            if !path.exists() {
                daemon_log::warn(format!(
                    "fghjd: skipping missing workspace {id} ({})",
                    path.display()
                ));
                continue;
            }
            match WorkspaceState::new(path.clone(), docker.clone()).await {
                Ok(state) => {
                    by_id.insert(id, Arc::new(state));
                }
                Err(e) => daemon_log::warn(format!(
                    "fghjd: failed to load workspace {id} ({}): {e}",
                    path.display()
                )),
            }
        }
        let registry = Self {
            by_id: Mutex::new(by_id),
            index_path,
            docker,
            actors: registry::ActorRegistry::new(),
            docker_converge_tasks: Mutex::new(HashMap::new()),
        };
        let loaded: Vec<(String, Arc<WorkspaceState>)> = registry
            .by_id
            .lock()
            .unwrap()
            .iter()
            .map(|(id, state)| (id.clone(), state.clone()))
            .collect();
        for (id, state) in loaded {
            registry.wire_actor(&id, &state);
        }
        registry
    }

    /// Spawns the new-system actor for `id`, seeded with a one-shot mirror
    /// of `old`'s live `runs::RunRegistry` (via
    /// `effects::docker::converge::mirror_runs`) so a freshly-wired
    /// workspace with pre-existing/persisted runs doesn't start out looking
    /// empty, then starts the `DockerConvergeEffect` task that's the only
    /// thing driving real Docker calls from here on — `effects::bridge`,
    /// which used to keep the new state live by re-polling
    /// `RunRegistry::list()` roughly once a second, is gone as of migration
    /// phase 5 (see `effects::docker::converge`'s module doc for the
    /// "no more bridge" tradeoff this leaves open). Called from both
    /// `load_from` (startup) and `resolve` (a fresh `fghj ui`/wire) — every
    /// workspace this registry ever registers also gets a wired actor,
    /// right at the exact place its old-system `Arc<WorkspaceState>` is
    /// born.
    fn wire_actor(&self, id: &str, old: &Arc<WorkspaceState>) {
        let seed = state::WorkspaceState {
            runs: effects::docker::converge::mirror_runs(&old.runs.list()),
            ..Default::default()
        };
        let handle = actor::spawn(seed);
        let docker_converge_task = tokio::spawn(effects::run_effect(
            effects::docker::DockerConvergeEffect::new(old.clone(), handle.clone()),
            handle.subscribe(),
            "docker_converge",
        ));
        self.actors
            .insert(id.to_string(), registry::WorkspaceHandle { actor: handle });
        self.docker_converge_tasks
            .lock()
            .unwrap()
            .insert(id.to_string(), docker_converge_task);
    }

    /// The daemon-wide directory of new-system actors this registry has
    /// wired — used by `DaemonControl::activate` to subscribe the raw-net
    /// fanned-in effect.
    pub fn actors(&self) -> &registry::ActorRegistry {
        &self.actors
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
        owner: Option<persistence::WorkspaceOwner>,
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
                self.wire_actor(&id, &state);

                let mut index = persistence::load_index(&self.index_path);
                index.insert(id.clone(), canonical.clone());
                persistence::save_index(&self.index_path, &index)?;
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

    /// Every `wildcard_hosts` suffix currently claimed by a `"running"`
    /// container in any wired workspace, sorted and deduplicated — the input
    /// to `dns::install_os_resolver_config`'s per-zone `/etc/resolver` sync,
    /// still called from here by `dns::ZoneSource::answer_for` (a live query
    /// path, not a converge effect — see `effects::dns`'s module doc for why
    /// that one path stays on the old system for now). Recomputed from
    /// scratch on every call (mirrors `resolve_route`'s own linear scan)
    /// rather than tracked incrementally.
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

    /// Every `"running"` container's `raw_domain` and published host ports
    /// in any wired workspace — used by `get_daemon_net_status` to map a
    /// raw-net virtual IP back to the domain it belongs to for telemetry.
    /// `effects::raw_net::RawNetEffect` computes its own equivalent
    /// projection off the new-system state instead of calling this. Same
    /// recompute-from-scratch approach as `active_wildcard_suffixes`.
    pub fn active_raw_endpoints(&self) -> Vec<raw_net::RawEndpoint> {
        let states: Vec<Arc<WorkspaceState>> =
            self.by_id.lock().unwrap().values().cloned().collect();
        states
            .iter()
            .flat_map(|state| {
                state.runs.list().into_iter().flat_map(|run| {
                    run.containers
                        .into_iter()
                        .filter(|c| c.status == "running")
                        .map(|c| raw_net::RawEndpoint {
                            raw_domain: c.raw_domain,
                            ports: c
                                .ports
                                .into_iter()
                                .filter_map(|(port, host_port)| Some((port, host_port?)))
                                .collect(),
                        })
                })
            })
            .collect()
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
                self.actors.remove(id);
                if let Some(task) = self.docker_converge_tasks.lock().unwrap().remove(id) {
                    task.abort();
                }
                for run in state.runs.list() {
                    let _ = state.runs.stop(&run.run_id).await;
                }
                let mut index = persistence::load_index(&self.index_path);
                index.remove(id);
                let _ = persistence::save_index(&self.index_path, &index);
                true
            }
            None => false,
        }
    }
}

impl proxy::RouteResolver for WorkspaceRegistry {
    fn resolve(&self, host: &str) -> Option<proxy::Backend> {
        self.resolve_route(host).map(|port| proxy::Backend {
            host: "127.0.0.1".to_string(),
            port,
        })
    }
}

impl dns::ZoneSource for WorkspaceRegistry {
    fn answer_for(&self, qname: &str) -> Option<Ipv4Addr> {
        if dns::in_zone(qname)
            || self
                .active_wildcard_suffixes()
                .iter()
                .any(|z| dns::matches_zone(qname, z))
        {
            Some(dns::ANSWER)
        } else if dns::matches_zone(qname, dns::ZONE_RAW) {
            Some(raw_net::resolve(qname))
        } else {
            None
        }
    }
}

#[derive(Deserialize)]
struct StartRequest {
    entry: Option<String>,
    workspace: Option<PathBuf>,
    #[serde(default)]
    owner: Option<persistence::WorkspaceOwner>,
}

#[derive(Deserialize)]
struct StopRequest {
    id: String,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

/// Extracts the new-system `actor::ActorHandle` for the workspace named by
/// `?workspace=<id>` — the migration-phase-4 counterpart of
/// `WorkspaceExtractor` for handlers that dispatch an `Action` instead of
/// calling `runs::RunRegistry` directly. Kept as its own extractor rather
/// than folded into `WorkspaceExtractor` (which every other handler still
/// uses unchanged) since only the three node-lifecycle handlers need it.
struct ActorExtractor(actor::ActorHandle);

impl FromRequestParts<Arc<WorkspaceRegistry>> for ActorExtractor {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<WorkspaceRegistry>,
    ) -> Result<Self, Self::Rejection> {
        let query = parts.uri.query().unwrap_or("");
        match query_param(query, "workspace").and_then(|id| state.actors().get(id)) {
            Some(handle) => Ok(ActorExtractor(handle.actor)),
            None => Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "unknown or missing ?workspace=<id>; POST /workspaces first" })),
            )
                .into_response()),
        }
    }
}

/// Maps an `ActionRejected` from a dispatched node-lifecycle request to its
/// HTTP response — the migration-phase-4 counterpart of `err_response` for
/// handlers on the new `actor::ActorHandle::dispatch` path.
/// `AlreadyInFlight` -> 409, matching today's `RunRegistry::begin_action`
/// rejection exactly (see the architecture plan's "HTTP handler contract").
/// `RunNotFound`/`NodeNotFound` -> 404: an improvement over the old path's
/// blanket 500 (`bail!("no such run: ..")` via `err_response`), now that
/// the reducer distinguishes the two cases explicitly.
fn action_rejected_response(err: crate::action::ActionRejected) -> Response {
    use crate::action::ActionRejected;
    let status = match err {
        ActionRejected::AlreadyInFlight => StatusCode::CONFLICT,
        ActionRejected::RunNotFound | ActionRejected::NodeNotFound => StatusCode::NOT_FOUND,
    };
    (
        status,
        Json(serde_json::json!({ "error": err.to_string() })),
    )
        .into_response()
}

/// Builds the 200 response for a successful node-lifecycle dispatch: the
/// freshly-published `ContainerInfo` for `run_id`/`node_id`, shimmed
/// (`run_view::legacy_container`) into the same flat shape `run_response`/
/// `get_runs` use, so every endpoint that ever returns a container serves
/// one consistent JSON shape rather than the new nested one here and the old
/// flat one everywhere else — per the architecture plan's "HTTP handler
/// contract". Falls back to a bare `{"ok": true}` in the (practically
/// unreachable, since `reduce` always leaves a container that hasn't been
/// removed by `ContainerActionSettled` in place) case the container isn't
/// found right after a successful dispatch.
fn container_response(actor: &actor::ActorHandle, run_id: &str, node_id: &str) -> Response {
    let current = actor.current();
    match current
        .runs
        .get(run_id)
        .and_then(|run| run.containers.get(node_id))
    {
        Some(container) => {
            Json(serde_json::json!(run_view::legacy_container(container))).into_response()
        }
        None => Json(serde_json::json!({ "ok": true })).into_response(),
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

async fn get_runs(ActorExtractor(actor): ActorExtractor) -> Response {
    Json(serde_json::json!(run_view::legacy_runs(&actor.current()))).into_response()
}

/// Builds the response for a successful `RunPlanned` dispatch: the shimmed
/// flat view (`run_view::legacy_run`) of whatever `run_id` names in the
/// actor's freshly-published state, per the same "respond once the reducer
/// has recorded intent, not once Docker has actually finished" contract
/// `dispatch_node_action` already uses (see the architecture plan's "HTTP
/// handler contract"). `RunPlanned`'s reducer arm always inserts an entry
/// under `run_id` (empty on a brand new run, top-up-preserved on an
/// existing one), so the `None` branch is practically unreachable — kept
/// only for symmetry with `container_response`.
fn run_response(actor: &actor::ActorHandle, run_id: &str) -> Response {
    match actor.current().runs.get(run_id) {
        Some(run) => Json(serde_json::json!(run_view::legacy_run(run))).into_response(),
        None => Json(serde_json::json!({ "ok": true, "run_id": run_id })).into_response(),
    }
}

/// Dispatches `Action::RunPlanned` through the workspace actor instead of
/// calling `runs::RunRegistry::start`/`ensure_running` directly — migration
/// phase 5's HTTP cutover. Unlike the old synchronous handler, this returns
/// as soon as the reducer has recorded the still-unfulfilled intent
/// (`RunState::pending_create`); `effects::docker::converge`'s
/// `DockerConvergeEffect` (already wired per-workspace, see
/// `WorkspaceRegistry::wire_actor`) is what actually resolves the graph and
/// calls Docker afterwards, reporting the result back via
/// `Action::RunCreateSettled`. This is the same async contract migration
/// phase 4 already gave node-lifecycle endpoints — run creation was the one
/// endpoint still on the old fully-synchronous path, purely because of the
/// JSON-shape mismatch `run_view` now closes, not because of anything about
/// creation itself that needed different timing.
///
/// A freshly-created run's first response (and the `GET /runs` polls
/// immediately after it) can therefore show 0 containers for as long as
/// convergence takes, where the old handler always returned the fully
/// populated result — the same "trust `pending_action`/poll for the rest"
/// model the UI already applies to node start/stop/delete, just not
/// something it has a "run is being created" affordance for yet
/// (`pending_create` is deliberately never serialized — see its doc).
async fn post_runs(ActorExtractor(actor): ActorExtractor, body: Bytes) -> Response {
    let spec: state::RunSpec = if body.is_empty() {
        state::RunSpec {
            run_id: None,
            flow: None,
        }
    } else {
        match serde_json::from_slice(&body) {
            Ok(s) => s,
            Err(e) => return bad_request(e),
        }
    };

    let run_id = runs::resolve_run_id(spec.run_id.as_deref());
    let action = action::Action::RunPlanned {
        run_id: run_id.clone(),
        plan: spec,
    };
    match actor.dispatch(action).await {
        Ok(()) => run_response(&actor, &run_id),
        Err(e) => action_rejected_response(e),
    }
}

/// Deliberately left on the old `WorkspaceExtractor` / `RunRegistry::stop`
/// path, unlike the three per-node handlers below — migration phase 4
/// ("HTTP handler contract" in the architecture plan) only names
/// `/nodes/{node}/start|stop|delete`, never whole-run stop, and for good
/// reason: `RunRegistry::stop` tears down the run's network, sidecar and
/// volumes and drops its `RunRegistry` entry outright, none of which the
/// `pending_action`-per-container model that `effects::docker::converge`
/// converges has any representation for. `Action::RunStopRequested`'s
/// reducer arm only marks each idle container `Stopping`; routing this
/// endpoint through it would leave the network/sidecar/volumes orphaned.
/// Giving whole-run teardown its own first-class action/effect is later
/// migration-phase work, not something to half-do here.
async fn post_run_stop(
    AxumPath(run_id): AxumPath<String>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state.runs.stop(&run_id).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => err_response(e),
    }
}

/// Dispatches `action` against `actor` for `run_id`/`node_id` and replies
/// per the architecture plan's "HTTP handler contract" — migration phase 4:
/// this returns as soon as the (pure, in-memory) reducer has recorded the
/// intent, not once Docker has actually finished; `effects::docker::converge`
/// (wired per-workspace in `daemon::WorkspaceRegistry::wire_actor`) is what
/// actually performs the Docker call afterwards, asynchronously.
async fn dispatch_node_action(
    actor: &actor::ActorHandle,
    run_id: String,
    node_id: String,
    action: action::Action,
) -> Response {
    match actor.dispatch(action).await {
        Ok(()) => container_response(actor, &run_id, &node_id),
        Err(e) => action_rejected_response(e),
    }
}

async fn post_run_node_start(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    ActorExtractor(actor): ActorExtractor,
) -> Response {
    let action = action::Action::RunNodeStartRequested {
        run_id: run_id.clone(),
        node_id: node_id.clone(),
    };
    dispatch_node_action(&actor, run_id, node_id, action).await
}

async fn post_run_node_stop(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    ActorExtractor(actor): ActorExtractor,
) -> Response {
    let action = action::Action::RunNodeStopRequested {
        run_id: run_id.clone(),
        node_id: node_id.clone(),
    };
    dispatch_node_action(&actor, run_id, node_id, action).await
}

async fn post_run_node_delete(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    ActorExtractor(actor): ActorExtractor,
) -> Response {
    let action = action::Action::RunNodeDeleteRequested {
        run_id: run_id.clone(),
        node_id: node_id.clone(),
    };
    dispatch_node_action(&actor, run_id, node_id, action).await
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

    let stream = docker::logs_follow(&state.docker, &container_name, false).map(|item| {
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

/// Lists the log generations `fghjd` has retained for this node (current and
/// previous, per the two-generation retention policy — see
/// `persistence::WorkspaceDb::begin_log_generation`), for the UI's generation
/// picker. Reads straight from the db regardless of whether the run/node is
/// still live, so history for a just-crashed or just-removed container
/// stays browsable.
async fn get_run_node_log_generations(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state.db.clone().list_log_generations(run_id, node_id).await {
        Ok(generations) => Json(serde_json::json!({ "generations": generations })).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Deserialize)]
struct LogHistoryQuery {
    generation: i64,
    before_seq: Option<i64>,
    #[serde(default = "default_log_history_limit")]
    limit: i64,
}

fn default_log_history_limit() -> i64 {
    500
}

/// Paginated log history for one generation, oldest-first — backs both the
/// initial load (`before_seq` omitted, returns the most recent `limit`
/// lines) and infinite scroll-back (`before_seq` set to the oldest `seq`
/// currently loaded).
async fn get_run_node_log_history(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    Query(q): Query<LogHistoryQuery>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state
        .db
        .clone()
        .load_log_history(run_id, node_id, q.generation, q.before_seq, q.limit)
        .await
    {
        Ok(lines) => Json(serde_json::json!({ "lines": lines })).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    action: String,
}

/// The current cycle's step-by-step narration of what `fghjd` itself did
/// for the last `start` or `stop` of this node — the ArgoCD-style "events"
/// counterpart to the raw container-log history above. Only ever the most
/// recent cycle of `action`: see `persistence::WorkspaceDb::begin_event_cycle`.
async fn get_run_node_events(
    AxumPath((run_id, node_id)): AxumPath<(String, String)>,
    Query(q): Query<EventsQuery>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match state
        .db
        .clone()
        .list_events(run_id, node_id, q.action)
        .await
    {
        Ok(events) => Json(serde_json::json!({ "events": events })).into_response(),
        Err(e) => err_response(e),
    }
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
            "/runs/{run_id}/nodes/{node_id}/logs/generations",
            get(get_run_node_log_generations),
        )
        .route(
            "/runs/{run_id}/nodes/{node_id}/logs/history",
            get(get_run_node_log_history),
        )
        .route(
            "/runs/{run_id}/nodes/{node_id}/events",
            get(get_run_node_events),
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
        .route("/daemon/logs", get(get_daemon_logs))
        .route("/daemon/net-status", get(get_daemon_net_status))
        .with_state(daemon);

    api.merge(daemon_api).fallback(static_handler)
}

/// `fghj daemon start` — reconciles `fghjd` back into the active state
/// (rebinds DNS/80/443, resyncs `/etc/hosts`). Idempotent: calling it while
/// already active just reports the current state back. Clears the
/// `idle_requested` flag in `persistence::DaemonState` on success so a later
/// crash/reboot restart comes back active too, instead of silently
/// reverting to idle.
async fn post_daemon_start(State(daemon): State<Arc<DaemonControl>>) -> Response {
    match daemon.activate().await {
        Ok(()) => {
            if let Err(e) = daemon.set_idle_requested(false) {
                daemon_log::warn(format!("fghjd: failed to persist daemon state: {e}"));
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
/// `persistence::DaemonState` so a crash or reboot before the next `start` doesn't
/// silently reactivate `fghjd` against the operator's wishes.
async fn post_daemon_stop(State(daemon): State<Arc<DaemonControl>>) -> Response {
    daemon.deactivate();
    if let Err(e) = daemon.set_idle_requested(true) {
        daemon_log::warn(format!("fghjd: failed to persist daemon state: {e}"));
    }
    Json(serde_json::json!({ "active": false })).into_response()
}

async fn get_daemon_status(State(daemon): State<Arc<DaemonControl>>) -> Response {
    Json(serde_json::json!({ "active": daemon.is_active() })).into_response()
}

#[derive(Deserialize)]
struct DaemonLogsQuery {
    after_seq: Option<u64>,
    #[serde(default = "default_daemon_logs_limit")]
    limit: usize,
}

fn default_daemon_logs_limit() -> usize {
    500
}

/// Recent `fghjd` process log lines (see `daemon_log`) — backs the
/// telemetry drawer's "Logs" tab. Polled rather than streamed (SSE): unlike
/// a container's stdout, this is low-volume, operator-facing lifecycle/
/// reconcile output, not app request logs — a short poll interval is just
/// as responsive and much simpler than a live stream. Takes no `State`
/// extractor since `daemon_log`'s ring buffer is process-global, not tied to
/// any particular `DaemonControl`.
async fn get_daemon_logs(Query(q): Query<DaemonLogsQuery>) -> Response {
    Json(serde_json::json!({ "entries": daemon_log::tail(q.after_seq, q.limit) })).into_response()
}

/// Snapshot of the three native-OS integration mechanisms
/// `effects::spawn_all`'s fanned-in effects maintain — `/etc/hosts`, macOS's
/// `/etc/resolver`, and the raw-zone virtual-IP NAT routes — read directly
/// from their actual on-disk/live state (not from what was last *computed*
/// as desired), so drift between "what fghjd wanted" and "what's actually
/// installed" would show up here. Backs the telemetry drawer's "DNS / DNAT"
/// tab.
async fn get_daemon_net_status(State(daemon): State<Arc<DaemonControl>>) -> Response {
    let hosts = hosts_file::managed_hosts(&hosts_file::hosts_path());
    let resolver_zones: Vec<_> = dns::managed_resolver_zones(Path::new("/etc/resolver"))
        .into_iter()
        .map(|(zone, port)| serde_json::json!({ "zone": zone, "port": port }))
        .collect();

    // `raw_net`'s own storage only keeps bare `RouteSpec`s (virtual IP +
    // ports, no domain) — the reverse virtual-IP -> raw-domain mapping is
    // done here, from the registry's current endpoints, rather than
    // plumbing a domain field through `raw_net`'s internal state.
    let domain_by_ip: HashMap<Ipv4Addr, String> = daemon
        .registry
        .active_raw_endpoints()
        .iter()
        .map(|e| (raw_net::resolve(&e.raw_domain), e.raw_domain.clone()))
        .collect();
    let raw_routes: Vec<_> = raw_net::current_routes()
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "virtual_ip": r.virtual_ip,
                "container_port": r.container_port,
                "host_port": r.host_port,
                "raw_domain": domain_by_ip.get(&r.virtual_ip),
            })
        })
        .collect();

    Json(serde_json::json!({
        "hosts": hosts,
        "resolver_zones": resolver_zones,
        "raw_routes": raw_routes,
        "last_reconcile_ms": daemon.last_reconcile_ms(),
    }))
    .into_response()
}

/// The pieces of `fghjd` that only exist while it's in the "active" state:
/// the DNS server, and the HTTP/HTTPS reverse proxy occupying 80/443.
/// Dropping (aborting) these tasks frees the ports/socket they held.
struct ActiveResources {
    dns_task: tokio::task::JoinHandle<()>,
    http_task: tokio::task::JoinHandle<()>,
    https_task: tokio::task::JoinHandle<()>,
    /// Drives `effects::raw_net::RawNetEffect`, `effects::dns::DnsEffect`,
    /// and `effects::hosts::HostsEffect` for as long as `fghjd` is active —
    /// the sole remaining callers of `raw_net::reconcile`,
    /// `dns::install_os_resolver_config`, and `hosts_file::sync` (see
    /// `spawn_reconciler`'s doc for why that function no longer calls any of
    /// them directly). Aborted on `deactivate`, so an idle `fghjd` never
    /// keeps converging pf routes, `/etc/resolver`, or `/etc/hosts`.
    effect_tasks: effects::EffectTasks,
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
    /// Epoch-millis timestamp of `spawn_reconciler`'s last completed
    /// container-status refresh tick while active — surfaced by
    /// `/daemon/net-status` so the telemetry drawer can show how fresh that
    /// data is. `/etc/hosts`/`/etc/resolver`/raw-net routes are no longer
    /// synced on this tick (see `spawn_reconciler`'s doc) — they converge
    /// continuously via `effects::spawn_all`'s fanned-in effects instead, so
    /// this field no longer reflects their freshness. `None` until the
    /// first tick after `fghjd` starts (or while idle).
    last_reconcile_ms: Mutex<Option<u64>>,
    /// Tracks that the operator's last explicit `fghj daemon` call was
    /// `stop`, not just that `fghjd` currently happens to be idle in memory.
    /// Read once at construction from `persistence::DaemonState` at
    /// `persistence::default_state_path()` — next to the CA (durable,
    /// survives a reboot) rather than under `/var/run` — then kept in memory
    /// and written through on every change via `set_idle_requested`; nothing
    /// else reads or writes `daemon-state.json` directly. "I told it to
    /// stop" is a standing instruction that should hold until countermanded
    /// by `fghj daemon start`, not something a crash or a reboot should
    /// silently discard by reactivating anyway. The path itself is a field
    /// (defaulting to `persistence::default_state_path()` in production)
    /// rather than hardcoded in `set_idle_requested`, so tests can point it
    /// at a temp file instead of the real, root-owned, production path.
    idle_requested: Mutex<bool>,
    daemon_state_path: PathBuf,
}

impl DaemonControl {
    pub fn is_active(&self) -> bool {
        self.active.lock().unwrap().is_some()
    }

    pub fn is_idle_requested(&self) -> bool {
        *self.idle_requested.lock().unwrap()
    }

    pub fn set_idle_requested(&self, idle_requested: bool) -> Result<()> {
        let path = &self.daemon_state_path;
        let mut state = persistence::load_daemon_state(path);
        state.idle_requested = idle_requested;
        persistence::save_daemon_state(path, &state)?;
        *self.idle_requested.lock().unwrap() = idle_requested;
        Ok(())
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
        let dns_task = tokio::spawn(dns::serve(dns_socket, self.registry.clone(), None));

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

        let effect_tasks = effects::spawn_all(dns_port, self.registry.actors().subscribe());

        *self.active.lock().unwrap() = Some(ActiveResources {
            dns_task,
            http_task,
            https_task,
            effect_tasks,
        });
        Ok(())
    }

    /// Epoch-millis of `spawn_reconciler`'s last completed tick, or
    /// `None` if it hasn't run yet. See the field doc for what this does and
    /// doesn't cover.
    pub fn last_reconcile_ms(&self) -> Option<u64> {
        *self.last_reconcile_ms.lock().unwrap()
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
            resources.effect_tasks.abort_all();
        }
        dns::clear_os_resolver_config();
        if let Err(e) = hosts_file::sync(&hosts_file::hosts_path(), &[]) {
            daemon_log::warn(format!(
                "fghjd: failed to clear /etc/hosts on deactivate: {e}"
            ));
        }
        if let Err(e) = raw_net::clear() {
            daemon_log::warn(format!(
                "fghjd: failed to clear raw-net routes on deactivate: {e}"
            ));
        }
    }
}

/// Background loop, analogous to a Kubernetes controller's reconcile loop
/// but read-only with respect to Docker: on each tick it re-inspects every
/// workspace's live containers and updates their recorded status, published
/// port, and routes (see `RunRegistry::refresh`) so drift caused by someone
/// `docker stop`/`rm`-ing a container by hand, or Docker itself moving a
/// container to a different ephemeral host port on a restart it initiated
/// (restart policy, `dockerd` restarting), shows up — and routes correctly —
/// on its own, without a `fghjd` restart. It never recreates or restarts a
/// container itself — no self-healing there.
///
/// This used to also own re-syncing `/etc/hosts`, macOS's `/etc/resolver`,
/// and raw-zone virtual-IP NAT routes directly off `daemon.registry`. All
/// three have since moved to the daemon-wide fanned-in effects
/// `effects::spawn_all` spawns from `DaemonControl::activate`
/// (`effects::hosts::HostsEffect`, `effects::dns::DnsEffect`,
/// `effects::raw_net::RawNetEffect`), driven off the new redux-style actor
/// state instead (see `effects::bridge`'s module doc for how that state
/// stays live) — see the architecture plan (rosy-soaring-teapot.md)'s
/// "dns + hosts_file effects" step. This loop and those effects must never
/// both write the same OS resource concurrently: two schedules touching the
/// same `pf`/`/etc/hosts`/`/etc/resolver` state is the exact bug class
/// documented in `raw_net::macos`'s module doc — that's why none of that
/// sync happens here any more.
fn spawn_reconciler(daemon: Arc<DaemonControl>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
        loop {
            interval.tick().await;
            for (id, _) in daemon.registry.list() {
                if let Some(state) = daemon.registry.get(&id) {
                    state.runs.refresh().await;
                    // Reports the freshly re-inspected status into the new
                    // actor system — see `effects::docker::observe`'s
                    // module doc for why this piggybacks on `refresh`'s
                    // own tick rather than polling Docker a second time.
                    if let Some(handle) = daemon.registry.actors().get(&id) {
                        effects::docker::observe::report(&state.runs, &handle.actor).await;
                    }
                }
            }
            if daemon.is_active() {
                *daemon.last_reconcile_ms.lock().unwrap() = Some(now_ms());
            }
        }
    });
}

/// The config-drift counterpart to `spawn_reconciler`: on its own, much
/// slower interval (`SYNC_RECONCILE_INTERVAL`), re-resolves each wired
/// workspace's `.fghj.yaml` from disk and asks its `RunRegistry` to compare
/// that freshly-resolved graph against the config each live container was
/// actually last started with (`RunRegistry::refresh_sync_status`). Runs
/// regardless of `daemon.is_active()` — sync status is informational graph
/// metadata, not a routing/`/etc/hosts` side effect, so there's no
/// "idle fghjd" reason to skip it the way `spawn_reconciler` skips its
/// `/etc/hosts`/`/etc/resolver` sync.
fn spawn_sync_reconciler(daemon: Arc<DaemonControl>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SYNC_RECONCILE_INTERVAL);
        loop {
            interval.tick().await;
            for (id, path) in daemon.registry.list() {
                let Some(state) = daemon.registry.get(&id) else {
                    continue;
                };
                let graph =
                    match tokio::task::spawn_blocking(move || resolver::resolve_universe(&path))
                        .await
                    {
                        Ok(Ok(graph)) => graph,
                        _ => continue,
                    };
                state.runs.refresh_sync_status(&graph).await;
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
    // `cert.pem`/`bundle.pem` under the same dir: the whole mechanism by
    // which a container trusts fghj's zone, via a plain `volumes:` mount in
    // its own `.fghj.yaml` — see `ca::refresh_trust_files`. The CA itself
    // never rotates at runtime, so this only needs to run once here, not on
    // a periodic reconciler.
    ca::refresh_trust_files(&ca_dir(), &ca).context("failed to refresh CA trust files")?;

    // Best-effort and non-blocking: not every workspace ends up starting a
    // run before `fghjd` itself might need to restart, so a slow/failed
    // build here (first build compiles the whole crate, and needs crates.io
    // reachable) shouldn't hold up `fghjd` starting or fail it outright — a
    // run that actually needs its sidecar will surface a real error from
    // `RunRegistry::ensure_sidecar`'s own call to this same function.
    {
        let docker = docker.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::sidecar_image::ensure_built(&docker).await {
                daemon_log::warn(format!(
                    "fghjd: failed to pre-build the sidecar proxy image: {e:#}"
                ));
            }
        });
    }

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

    let daemon_state_path = persistence::default_state_path();
    let idle_requested = persistence::load_daemon_state(&daemon_state_path).idle_requested;
    let daemon = Arc::new(DaemonControl {
        registry: registry.clone(),
        cert_resolver,
        provider,
        control_port,
        active: Mutex::new(None),
        last_reconcile_ms: Mutex::new(None),
        idle_requested: Mutex::new(idle_requested),
        daemon_state_path,
    });
    spawn_reconciler(Arc::clone(&daemon));
    spawn_sync_reconciler(Arc::clone(&daemon));

    // `fghjd` starts active by default: it's meant to occupy 80/443 and
    // *.fghj.internal DNS from the moment the system boots. The one
    // exception is `daemon.is_idle_requested()` — if the operator's last
    // explicit `fghj daemon` call was `stop`, a crash or reboot in between
    // must not silently override that by reactivating anyway; staying idle
    // here is what makes `fghj daemon stop` a durable instruction rather
    // than a one-shot action that a flaky Docker daemon or a reboot can undo
    // behind the operator's back. A bind failure during activation (e.g.
    // "something else is already listening on 80/443") still fails startup
    // fast, before the control API ever serves a request.
    if daemon.is_idle_requested() {
        daemon_log::info(
            "fghjd: starting idle — last `fghj daemon` action was `stop`; run `fghj daemon start` to reconcile"
                .to_string(),
        );
    } else {
        daemon.activate().await?;
    }

    let app = build_router(registry, Arc::clone(&daemon));
    daemon_log::info(format!(
        "fghjd: control API listening on {} (CLI) and reachable via https://{}",
        socket_path.display(),
        dns::ZONE
    ));

    // Same router, two listeners: the Unix socket is the CLI's channel, the
    // TCP one is only ever dialed internally by the HTTPS proxy's apex-name
    // relay (see above) — `Router` is cheap to clone (an `Arc` internally).
    let cli_app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(cli_listener, cli_app).await {
            daemon_log::warn(format!("fghjd: control socket server error: {e}"));
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
            daemon_log::info(
                "fghjd: received shutdown signal, releasing ports and cleaning up...".to_string(),
            );
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
        let persisted = persistence::load_index(&tmp.path().join("workspaces.json"));
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

    async fn test_daemon_control(state_path: PathBuf) -> Arc<DaemonControl> {
        let tmp = tempfile::tempdir().unwrap();
        let registry = Arc::new(
            WorkspaceRegistry::load_from(tmp.path().join("workspaces.json"), test_docker()).await,
        );
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cert_resolver = Arc::new(ca::DynamicCertResolver::new(
            ca::generate_ca_for_tests(),
            provider.clone(),
            registry.clone(),
        ));
        let idle_requested = persistence::load_daemon_state(&state_path).idle_requested;
        Arc::new(DaemonControl {
            registry,
            cert_resolver,
            provider,
            control_port: 0,
            active: Mutex::new(None),
            last_reconcile_ms: Mutex::new(None),
            idle_requested: Mutex::new(idle_requested),
            daemon_state_path: state_path,
        })
    }

    #[tokio::test]
    async fn idle_requested_is_cached_in_memory_and_written_through_on_change() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("daemon-state.json");
        let daemon = test_daemon_control(state_path.clone()).await;

        // Read once at construction: a freshly-started daemon with no prior
        // `daemon stop` comes back active, matching the on-disk default.
        assert!(!daemon.is_idle_requested());

        daemon.set_idle_requested(true).unwrap();
        assert!(
            daemon.is_idle_requested(),
            "in-memory cache must reflect the change immediately"
        );
        assert!(
            persistence::load_daemon_state(&state_path).idle_requested,
            "the change must be written through to disk, not just cached in memory"
        );

        // Mutating the file directly (simulating some other process) must
        // NOT be observed without going through `set_idle_requested` —
        // `idle_requested` is read once at boot, not re-read live.
        persistence::save_daemon_state(
            &state_path,
            &persistence::DaemonState {
                idle_requested: false,
            },
        )
        .unwrap();
        assert!(
            daemon.is_idle_requested(),
            "the in-memory cache must not silently pick up an out-of-band disk change"
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
        assert!(!persistence::load_index(&tmp.path().join("workspaces.json")).contains_key(&id));
        // stopping an unknown id is reported, not a panic
        assert!(!registry.stop(&id).await);
    }
}
