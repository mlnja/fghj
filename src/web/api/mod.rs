//! The axum router. `/daemon/*` holds `DaemonControl`; everything else holds
//! the `WorkspaceRegistry`; the rest falls through to the embedded UI.

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};

pub mod control;
pub mod downloads;
pub mod env;
pub mod error;
pub mod exec;
pub mod extract;
pub mod logs;
pub mod repos;
pub mod traffic;
pub mod workspaces;

use crate::daemon::control::DaemonControl;
use crate::daemon::registry::WorkspaceRegistry;
use control::{
    get_daemon_doctor, get_daemon_logs, get_daemon_net_status, get_daemon_status,
    post_daemon_docker, post_daemon_sidecars_remove, post_daemon_start, post_daemon_stop,
    post_daemon_trust,
};
use downloads::{
    get_activities, get_pull_all_status, get_pull_jobs, get_pull_node_status, post_pull_all,
    post_pull_node,
};
use env::{
    get_environment, post_node_debug_wait, post_node_delete, post_node_start, post_node_stop,
    post_switch,
};
use exec::get_node_exec_ws;
use logs::{
    get_node_events, get_node_logs, get_node_logs_stream, get_run_node_log_generations,
    get_run_node_log_history,
};
use workspaces::{get_universe, get_workspaces, post_workspaces, post_workspaces_remove};

use crate::web::ui::static_handler;

pub(crate) fn build_router(registry: Arc<WorkspaceRegistry>, daemon: Arc<DaemonControl>) -> Router {
    let api = Router::new()
        .route("/workspaces", get(get_workspaces).post(post_workspaces))
        .route("/workspaces/remove", post(post_workspaces_remove))
        .route("/universe.json", get(get_universe))
        .route("/pull-all", post(post_pull_all))
        .route("/pull-all/status", get(get_pull_all_status))
        .route("/pull/{node_id}", post(post_pull_node))
        .route("/pull/{node_id}/status", get(get_pull_node_status))
        .route("/pull-jobs", get(get_pull_jobs))
        .route("/activities", get(get_activities))
        .route("/repos", get(repos::get_repos))
        .route("/repos/{path}/fetch", post(repos::post_repo_fetch))
        .route("/repos/{path}/pull", post(repos::post_repo_pull))
        .route("/environment", get(get_environment))
        .route("/switch", post(post_switch))
        .route("/nodes/{node_id}/start", post(post_node_start))
        .route("/nodes/{node_id}/stop", post(post_node_stop))
        .route("/nodes/{node_id}/delete", post(post_node_delete))
        .route("/nodes/{node_id}/debug-wait", post(post_node_debug_wait))
        .route("/nodes/{node_id}/logs", get(get_node_logs))
        .route("/nodes/{node_id}/logs/stream", get(get_node_logs_stream))
        .route(
            "/nodes/{node_id}/logs/generations",
            get(get_run_node_log_generations),
        )
        .route(
            "/nodes/{node_id}/logs/history",
            get(get_run_node_log_history),
        )
        .route("/nodes/{node_id}/events", get(get_node_events))
        .route("/nodes/{node_id}/exec/ws", get(get_node_exec_ws))
        .merge(traffic::routes())
        .route("/daemon/sidecars/remove", post(post_daemon_sidecars_remove))
        .with_state(registry);

    let daemon_api = Router::new()
        .route("/daemon/start", post(post_daemon_start))
        .route("/daemon/stop", post(post_daemon_stop))
        .route("/daemon/docker", post(post_daemon_docker))
        .route("/daemon/trust", post(post_daemon_trust))
        .route("/daemon/status", get(get_daemon_status))
        .route("/daemon/logs", get(get_daemon_logs))
        .route("/daemon/net-status", get(get_daemon_net_status))
        .route("/daemon/doctor", get(get_daemon_doctor))
        .with_state(daemon);

    api.merge(daemon_api).fallback(static_handler)
}
