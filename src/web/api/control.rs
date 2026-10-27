use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::daemon::control::DaemonControl;
use crate::state::query;
use crate::web::proxy;
use crate::{daemon_log, doctor, hosts_file, platform, raw_net};

/// `fghj daemon start`: rebind DNS/80/443. Idempotent; persists the choice so a
/// restart comes back active.
pub(crate) async fn post_daemon_start(State(daemon): State<Arc<DaemonControl>>) -> Response {
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

/// `fghj daemon stop`: release 80/443/DNS but keep serving this API; containers
/// are left alone. Persisted so a restart stays idle.
pub(crate) async fn post_daemon_stop(State(daemon): State<Arc<DaemonControl>>) -> Response {
    daemon.deactivate();
    if let Err(e) = daemon.set_idle_requested(true) {
        daemon_log::warn(format!("fghjd: failed to persist daemon state: {e}"));
    }
    Json(serde_json::json!({ "active": false })).into_response()
}

pub(crate) async fn get_daemon_status(State(daemon): State<Arc<DaemonControl>>) -> Response {
    Json(serde_json::json!({ "active": daemon.is_active() })).into_response()
}

#[derive(Deserialize)]
pub(crate) struct DaemonLogsQuery {
    after_seq: Option<u64>,
    #[serde(default = "default_daemon_logs_limit")]
    limit: usize,
}

pub(crate) fn default_daemon_logs_limit() -> usize {
    500
}

pub(crate) async fn get_daemon_logs(Query(q): Query<DaemonLogsQuery>) -> Response {
    Json(serde_json::json!({ "entries": daemon_log::tail(q.after_seq, q.limit) })).into_response()
}

/// The OS integrations as actually installed, so drift from what fghjd wanted
/// shows up.
pub(crate) async fn get_daemon_net_status(State(daemon): State<Arc<DaemonControl>>) -> Response {
    let hosts = hosts_file::managed_hosts(&hosts_file::hosts_path());
    let resolver_zones: Vec<_> = platform::current()
        .dns
        .status()
        .map(|r| r.zones)
        .unwrap_or_default()
        .into_iter()
        .map(|(zone, port)| serde_json::json!({ "zone": zone, "port": port }))
        .collect();

    let domain_by_ip: HashMap<Ipv4Addr, String> = query::raw_endpoints(&daemon.registry.states())
        .into_iter()
        .map(|e| (raw_net::resolve(&e.raw_domain), e.raw_domain))
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
        // Reported, not assumed: the `/etc/hosts` entries point here, not 127.0.0.1.
        "proxy_ip": proxy::PROXY_IP,
        "last_reconcile_ms": daemon.last_reconcile_ms(),
    }))
    .into_response()
}

/// Runs in the daemon: the useful checks need its Docker client, activation
/// state and root.
pub(crate) async fn get_daemon_doctor(State(daemon): State<Arc<DaemonControl>>) -> Response {
    let docker = daemon.registry.docker();
    Json(doctor::daemon_report(&docker, &daemon).await).into_response()
}
