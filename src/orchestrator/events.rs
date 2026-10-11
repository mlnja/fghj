//! //! Operator-facing narration of an action, per (node, action) cycle, for the
//! //! UI's events pane. Best-effort: a failure to record is printed, never fatal.

use std::fmt::Display;

use super::registry::Orchestrator;

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
    /// Replaces this action's previous narration.
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

    pub(super) async fn failed(&self, step: &str, detail: impl Display) {
        self.record(step, "error", Some(format!("{detail:#}")))
            .await;
    }

    /// Skips recording if an inner step of this cycle already did, so the error
    /// isn't repeated under the wrong step.
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
