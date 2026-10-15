//! Pulls each workspace's sidecar traffic into the registry's
//! [`TrafficStore`](crate::web::traffic::TrafficStore) while any of its nodes
//! is recording. See `web::capture` for the link itself.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{Notify, watch};

use crate::orchestrator::route_table::sidecar_routes_dir;
use crate::state::EnvState;
use crate::web::capture::{self, CAPTURE_PORT, Modes, TOKEN_FILE};
use crate::{daemon_log, docker, supervisor};

use super::WorkspaceRegistry;

/// Also how soon a sidecar that came up is noticed.
const RECHECK: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(1);

pub fn spawn(registry: Arc<WorkspaceRegistry>) {
    supervisor::supervise_forever("sidecar traffic capture", run(registry));
}

struct Session {
    modes: watch::Sender<Modes>,
    task: tokio::task::AbortHandle,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(registry: Arc<WorkspaceRegistry>) {
    let woken = registry.traffic().modes_changed();
    let mut sessions: HashMap<String, Session> = HashMap::new();
    loop {
        sync(&registry, &mut sessions);
        tokio::select! {
            _ = woken.notified() => {}
            _ = tokio::time::sleep(RECHECK) => {}
        }
    }
}

/// One session per workspace with a recording node and a network, holding
/// the set it sends up. Dropping a session hangs up, which stops the sidecar.
fn sync(registry: &Arc<WorkspaceRegistry>, sessions: &mut HashMap<String, Session>) {
    let states = registry.states();
    sessions.retain(|ws, _| states.contains_key(ws));
    for (ws, state) in &states {
        let modes = registry.traffic().recording(ws);
        let networked = state
            .env
            .as_ref()
            .is_some_and(|env| !env.network.is_empty());
        if modes.is_empty() || !networked {
            sessions.remove(ws);
            continue;
        }
        match sessions.get(ws) {
            Some(session) => {
                session.modes.send_if_modified(|current| {
                    let changed = *current != modes;
                    *current = modes;
                    changed
                });
            }
            None => {
                let (tx, rx) = watch::channel(modes);
                let task = tokio::spawn(pull_forever(registry.clone(), ws.clone(), rx));
                sessions.insert(
                    ws.clone(),
                    Session {
                        modes: tx,
                        task: task.abort_handle(),
                    },
                );
            }
        }
    }
}

async fn pull_forever(registry: Arc<WorkspaceRegistry>, ws: String, modes: watch::Receiver<Modes>) {
    let mut last_error = String::new();
    loop {
        if let Err(e) = pull_once(&registry, &ws, modes.clone()).await {
            // Once per distinct reason: a sidecar that's down stays down for a while.
            let error = format!("{e:#}");
            if error != last_error {
                daemon_log::warn(format!(
                    "fghjd: no container-to-container traffic for workspace {ws}: {error}"
                ));
                last_error = error;
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

async fn pull_once(
    registry: &Arc<WorkspaceRegistry>,
    ws: &str,
    modes: watch::Receiver<Modes>,
) -> Result<()> {
    let state = registry
        .states()
        .remove(ws)
        .context("the workspace is gone")?;
    let env = state
        .env
        .clone()
        .context("the workspace has no environment")?;
    let docker = registry.docker();
    let status = docker::inspect_status(&docker, &env.sidecar_container_name)
        .await?
        .filter(|s| s.status == "running")
        .context("its sidecar isn't running")?;
    let port = status.host_port(&CAPTURE_PORT.to_string()).context(
        "its sidecar predates traffic capture; it gets one when the environment is recreated",
    )?;
    let token = std::fs::read_to_string(sidecar_routes_dir(&env.network).join(TOKEN_FILE))
        .context("failed to read the sidecar's capture token")?;

    let names = SourceNames::spawn(registry.clone(), ws.to_string());
    let traffic = registry.traffic();
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    capture::pull(addr, token.trim(), modes, |origin, mut shipped| {
        if let Some(node) = names.name(shipped.source()) {
            shipped.set_source(node);
        }
        traffic.log(ws, &shipped.node).ingest(origin, shipped);
    })
    .await
}

/// Names a calling container's IP after its node. Looked up off the ingest
/// path: an exchange is shipped again on each step, so the name lands on a
/// later one when the first misses.
struct SourceNames {
    by_ip: Arc<Mutex<HashMap<String, String>>>,
    wanted: Arc<Notify>,
    task: tokio::task::AbortHandle,
}

impl SourceNames {
    /// Read fresh each time, so a node created since still gets its name.
    fn spawn(registry: Arc<WorkspaceRegistry>, ws: String) -> Self {
        let by_ip = Arc::new(Mutex::new(HashMap::new()));
        let wanted = Arc::new(Notify::new());
        wanted.notify_one();
        let task = tokio::spawn({
            let (by_ip, wanted) = (by_ip.clone(), wanted.clone());
            async move {
                loop {
                    wanted.notified().await;
                    if let Some(ips) = lookup(&registry, &ws).await {
                        *by_ip.lock().unwrap() = ips;
                    }
                    // A caller that isn't a node would otherwise ask on every exchange.
                    tokio::time::sleep(RECHECK).await;
                }
            }
        });
        Self {
            by_ip,
            wanted,
            task: task.abort_handle(),
        }
    }

    fn name(&self, source: &str) -> Option<String> {
        let found = self.by_ip.lock().unwrap().get(source).cloned();
        if found.is_none() && source.parse::<Ipv4Addr>().is_ok() {
            self.wanted.notify_one();
        }
        found
    }
}

/// Node id by IP, for every node on the workspace's network.
async fn lookup(registry: &WorkspaceRegistry, ws: &str) -> Option<HashMap<String, String>> {
    let env: EnvState = registry.states().remove(ws)?.env.clone()?;
    let ips = docker::network_container_ips(&registry.docker(), &env.network)
        .await
        .ok()?;
    Some(
        ips.into_iter()
            .filter_map(|(ip, container)| {
                let node = env
                    .containers
                    .values()
                    .find(|c| c.desired.container_name == container)?;
                Some((ip, node.node_id.clone()))
            })
            .collect(),
    )
}

impl Drop for SourceNames {
    fn drop(&mut self) {
        self.task.abort();
    }
}
