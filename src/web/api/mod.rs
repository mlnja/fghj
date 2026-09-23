//! The axum router: every route the daemon serves, in one table.
//!
//! Two sub-routers, split by what they carry as axum state — `/workspaces`,
//! `/runs`, `/pull*` are per-workspace and hold the `WorkspaceRegistry`;
//! `/daemon/*` is about the daemon process itself and holds `DaemonControl`.
//! Anything neither claims falls through to the embedded UI
//! (`web::ui::static_handler`).

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};

pub mod control;
pub mod downloads;
pub mod error;
pub mod exec;
pub mod extract;
pub mod logs;
pub mod runs;
pub mod workspaces;

use crate::daemon::control::DaemonControl;
use crate::daemon::registry::WorkspaceRegistry;
use control::{
    get_daemon_logs, get_daemon_net_status, get_daemon_status, post_daemon_start, post_daemon_stop,
};
use downloads::{
    get_pull_all_status, get_pull_jobs, get_pull_node_status, post_pull_all, post_pull_node,
};
use exec::get_run_exec_ws;
use logs::{
    get_run_logs, get_run_logs_stream, get_run_node_events, get_run_node_log_generations,
    get_run_node_log_history,
};
use runs::{
    get_runs, post_run_node_delete, post_run_node_start, post_run_node_stop, post_run_stop,
    post_runs,
};
use workspaces::{get_universe, get_workspaces, post_workspaces, post_workspaces_stop};

use crate::web::ui::static_handler;

pub(crate) fn build_router(registry: Arc<WorkspaceRegistry>, daemon: Arc<DaemonControl>) -> Router {
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
