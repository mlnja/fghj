//! Operator-facing narration of what a run action is doing, recorded
//! per (run, node, action) cycle and read back by the UI's events pane.

use super::registry::RunRegistry;

impl RunRegistry {
    /// Clears out the previous cycle of `action` ("start" or "stop") for
    /// `node_id`, so the step-by-step narration `record_event` appends next
    /// is the only one a caller of the `/events` endpoint sees — per the
    /// explicit "new start overrides old start, new stop overrides old
    /// stop" requirement. Best-effort: a failure here only means a later
    /// `record_event` call might append onto a stale cycle instead of a
    /// fresh one, which isn't worth failing the actual start/stop over.
    pub(super) async fn begin_event_cycle(&self, run_id: &str, node_id: &str, action: &str) {
        if let Err(e) = self
            .db
            .clone()
            .begin_event_cycle(run_id.to_string(), node_id.to_string(), action.to_string())
            .await
        {
            eprintln!("fghjd: failed to begin {action} event cycle for node {node_id}: {e:#}");
        }
    }

    /// Appends one orchestration-level step (image build, container
    /// creation, healthcheck wait, ...) to the current `action` cycle for
    /// `node_id` — the ArgoCD-style "events" narration of what `fghjd`
    /// itself is doing, distinct from the container's own stdout/stderr
    /// captured by `spawn_log_capture`. Best-effort, same reasoning as
    /// `begin_event_cycle`: a logging failure shouldn't fail the actual
    /// start/stop.
    pub(super) async fn record_event(
        &self,
        run_id: &str,
        node_id: &str,
        action: &str,
        step: &str,
        status: &str,
        detail: Option<String>,
    ) {
        if let Err(e) = self
            .db
            .clone()
            .append_event(
                run_id.to_string(),
                node_id.to_string(),
                action.to_string(),
                step.to_string(),
                status.to_string(),
                detail,
            )
            .await
        {
            eprintln!("fghjd: failed to record {action} event '{step}' for node {node_id}: {e:#}");
        }
    }

    /// Records `step` as failed, unless some inner step of this same cycle
    /// already recorded a failure.
    ///
    /// For the outer handlers that wrap a sequence of narrated steps. The
    /// error they catch is usually the one the inner step already reported,
    /// and recording it again puts the same text in the pane twice under the
    /// wrong step name. When nothing inner reported — `resolve_node_spec`
    /// has `bail!` paths that don't, like a backing node with no image or an
    /// unreadable `env_file` — this is the only thing that will, so it still
    /// has to run.
    ///
    /// Best-effort in the same way as `record_event`: if the check itself
    /// fails, record, on the grounds that a duplicate line is a much smaller
    /// problem than a failure nothing narrates at all.
    pub(super) async fn record_error_unless_reported(
        &self,
        run_id: &str,
        node_id: &str,
        action: &str,
        step: &str,
        detail: String,
    ) {
        let already = self
            .db
            .clone()
            .cycle_has_error(run_id.to_string(), node_id.to_string(), action.to_string())
            .await
            .unwrap_or(false);
        if already {
            return;
        }
        self.record_event(run_id, node_id, action, step, "error", Some(detail))
            .await;
    }
}
