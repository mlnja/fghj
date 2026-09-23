//! The `/daemon/*` endpoints: start, stop, status, logs, net-status.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::daemon::control::DaemonControl;
use crate::state::query;
use crate::{daemon_log, dns, hosts_file, raw_net};

/// `fghj daemon start` — reconciles `fghjd` back into the active state
/// (rebinds DNS/80/443, resyncs `/etc/hosts`). Idempotent: calling it while
/// already active just reports the current state back. Clears the
/// `idle_requested` flag in `persistence::DaemonState` on success so a later
/// crash/reboot restart comes back active too, instead of silently
/// reverting to idle.
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

/// `fghj daemon stop` — releases 80/443/DNS and clears `/etc/hosts` without
/// touching the `fghjd` process itself (it keeps serving this control API so
/// a later `fghj daemon start` can reach it). Docker containers already
/// running are left alone. Also persists `idle_requested` in
/// `persistence::DaemonState` so a crash or reboot before the next `start` doesn't
/// silently reactivate `fghjd` against the operator's wishes.
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

/// Recent `fghjd` process log lines (see `daemon_log`) — backs the
/// telemetry drawer's "Logs" tab. Polled rather than streamed (SSE): unlike
/// a container's stdout, this is low-volume, operator-facing lifecycle/
/// reconcile output, not app request logs — a short poll interval is just
/// as responsive and much simpler than a live stream. Takes no `State`
/// extractor since `daemon_log`'s ring buffer is process-global, not tied to
/// any particular `DaemonControl`.
pub(crate) async fn get_daemon_logs(Query(q): Query<DaemonLogsQuery>) -> Response {
    Json(serde_json::json!({ "entries": daemon_log::tail(q.after_seq, q.limit) })).into_response()
}

/// Snapshot of the three native-OS integration mechanisms
/// `effects::spawn_all`'s fanned-in effects maintain — `/etc/hosts`, macOS's
/// `/etc/resolver`, and the raw-zone virtual-IP NAT routes — read directly
/// from their actual on-disk/live state (not from what was last *computed*
/// as desired), so drift between "what fghjd wanted" and "what's actually
/// installed" would show up here. Backs the telemetry drawer's "DNS / DNAT"
/// tab.
pub(crate) async fn get_daemon_net_status(State(daemon): State<Arc<DaemonControl>>) -> Response {
    let hosts = hosts_file::managed_hosts(&hosts_file::hosts_path());
    let resolver_zones: Vec<_> = dns::managed_resolver_zones(Path::new("/etc/resolver"))
        .into_iter()
        .map(|(zone, port)| serde_json::json!({ "zone": zone, "port": port }))
        .collect();

    // `raw_net`'s own storage only keeps bare `RouteSpec`s (virtual IP +
    // ports, no domain) — the reverse virtual-IP -> raw-domain mapping is
    // done here, from the registry's current endpoints, rather than
    // plumbing a domain field through `raw_net`'s internal state.
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
        "last_reconcile_ms": daemon.last_reconcile_ms(),
    }))
    .into_response()
}
