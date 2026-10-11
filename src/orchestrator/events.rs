//! Operator-facing narration of what an action is doing, recorded per
//! (node, action) cycle and read back by the UI's events pane. Distinct from
//! the container's own output, which `spawn_log_capture` records.
//!
//! Best-effort throughout: a failure to record is printed, never allowed to
//! fail the start or stop it narrates.

use std::fmt::Display;

use super::registry::Orchestrator;

/// The narration of one `action` ("start", "stop", "create") on one node.
pub(super) struct Events<'a> {
    orchestrator: &'a Orchestrator,
    node_id: &'a str,
    action: &'static str,
}

impl Orchestrator {
    pub(super) fn events<'a>(&'a self, node_id: &'a str, action: &'static str) -> Events<'a> {
        Events {
            orchestrator: self,
            node_id,
            action,
        }
    }
}

impl Events<'_> {
    /// Drops the previous cycle of this action: a new start replaces the
    /// last start's narration, a new stop the last stop's.
    pub(super) async fn begin(&self) {
        let db = self.orchestrator.db.clone();
        if let Err(e) = db
            .begin_event_cycle(self.node_id.to_string(), self.action.to_string())
            .await
        {
            self.report(e);
        }
    }

    pub(super) async fn running(&self, step: &str) {
        self.record(step, "running", None).await;
    }

    pub(super) async fn ok(&self, step: &str) {
        self.record(step, "ok", None).await;
    }

    /// `{detail:#}`, so an `anyhow::Error` records its whole context chain.
    pub(super) async fn failed(&self, step: &str, detail: impl Display) {
        self.record(step, "error", Some(format!("{detail:#}")))
            .await;
    }

    /// `failed`, unless an inner step of this cycle already recorded the
    /// failure — so an outer handler doesn't repeat it under the wrong step.
    /// If the check itself fails, record anyway: a duplicate line beats a
    /// failure nothing narrates.
    pub(super) async fn failed_unless_reported(&self, step: &str, detail: impl Display) {
        let db = self.orchestrator.db.clone();
        let already = db
            .cycle_has_error(self.node_id.to_string(), self.action.to_string())
            .await
            .unwrap_or(false);
        if !already {
            self.failed(step, detail).await;
        }
    }

    pub(super) async fn record(&self, step: &str, status: &str, detail: Option<String>) {
        let db = self.orchestrator.db.clone();
        if let Err(e) = db
            .append_event(
                self.node_id.to_string(),
                self.action.to_string(),
                step.to_string(),
                status.to_string(),
                detail,
            )
            .await
        {
            self.report(e);
        }
    }

    fn report(&self, e: anyhow::Error) {
        eprintln!(
            "fghjd: failed to record {} event for node {}: {e:#}",
            self.action, self.node_id
        );
    }
}
