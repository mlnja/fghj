use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::WorkspaceDb;

/// One orchestration-level step recorded during a `"start"` or `"stop"`
/// cycle — see [`WorkspaceDb`]'s `events` table. Distinct from a log line:
/// this is `fghjd` narrating what *it* is doing (building the image,
/// creating the container, waiting for a healthcheck), not the container's
/// own stdout/stderr.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEntry {
    pub seq: i64,
    pub ts: i64,
    pub step: String,
    pub status: String,
    pub detail: Option<String>,
}

impl WorkspaceDb {
    /// Clears any events left from a previous cycle of `action` (`"start"`
    /// or `"stop"`) for `(run_id, node_id)`. Unlike `logs`, events keep no
    /// generation history at all — per the explicit "new start overrides
    /// old start, new stop overrides old stop" requirement, only the single
    /// most recent cycle of each action is ever worth keeping, so the old
    /// one is simply deleted rather than pruned down to N generations.
    pub async fn begin_event_cycle(
        self: Arc<Self>,
        run_id: String,
        node_id: String,
        action: String,
    ) -> Result<()> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "DELETE FROM events WHERE run_id = ?1 AND node_id = ?2 AND action = ?3",
                rusqlite::params![run_id, node_id, action],
            )?;
            Ok(())
        })
        .await
        .context("begin_event_cycle task panicked")?
    }

    /// Appends one step to the cycle opened by [`Self::begin_event_cycle`].
    /// `status` is a small open-ended vocabulary (`"running"`, `"ok"`,
    /// `"error"`) rather than an enum, matching the informal, human-facing
    /// nature of these entries — see `runs::RunRegistry::record_event`, the
    /// only caller.
    pub async fn append_event(
        self: Arc<Self>,
        run_id: String,
        node_id: String,
        action: String,
        step: String,
        status: String,
        detail: Option<String>,
    ) -> Result<()> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let next_seq: i64 = conn.query_row(
                "SELECT COALESCE(MAX(seq) + 1, 0) FROM events WHERE run_id = ?1 AND node_id = ?2 AND action = ?3",
                rusqlite::params![run_id, node_id, action],
                |row| row.get(0),
            )?;
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            conn.execute(
                "INSERT INTO events (run_id, node_id, action, seq, ts, step, status, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![run_id, node_id, action, next_seq, ts, step, status, detail],
            )?;
            Ok(())
        })
        .await
        .context("append_event task panicked")?
    }

    /// Lists the steps of the current cycle of `action` for `(run_id,
    /// node_id)`, oldest first.
    pub async fn list_events(
        self: Arc<Self>,
        run_id: String,
        node_id: String,
        action: String,
    ) -> Result<Vec<EventEntry>> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT seq, ts, step, status, detail FROM events
                 WHERE run_id = ?1 AND node_id = ?2 AND action = ?3
                 ORDER BY seq ASC",
            )?;
            let rows = stmt.query_map(rusqlite::params![run_id, node_id, action], |row| {
                Ok(EventEntry {
                    seq: row.get(0)?,
                    ts: row.get(1)?,
                    step: row.get(2)?,
                    status: row.get(3)?,
                    detail: row.get(4)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into)
        })
        .await
        .context("list_events task panicked")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn events_are_replaced_wholesale_by_the_next_cycle_of_the_same_action() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        let run_id = "default".to_string();
        let node_id = "svc-a".to_string();

        db.clone()
            .begin_event_cycle(run_id.clone(), node_id.clone(), "start".to_string())
            .await
            .unwrap();
        db.clone()
            .append_event(
                run_id.clone(),
                node_id.clone(),
                "start".to_string(),
                "resolving config".to_string(),
                "running".to_string(),
                None,
            )
            .await
            .unwrap();
        db.clone()
            .append_event(
                run_id.clone(),
                node_id.clone(),
                "start".to_string(),
                "resolving config".to_string(),
                "ok".to_string(),
                None,
            )
            .await
            .unwrap();

        // A "stop" cycle for the same node is a completely independent
        // sequence — it must not disturb the "start" events above.
        db.clone()
            .begin_event_cycle(run_id.clone(), node_id.clone(), "stop".to_string())
            .await
            .unwrap();
        db.clone()
            .append_event(
                run_id.clone(),
                node_id.clone(),
                "stop".to_string(),
                "stopping container".to_string(),
                "ok".to_string(),
                None,
            )
            .await
            .unwrap();

        let start_events = db
            .clone()
            .list_events(run_id.clone(), node_id.clone(), "start".to_string())
            .await
            .unwrap();
        assert_eq!(
            start_events
                .iter()
                .map(|e| (e.seq, e.status.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "running"), (1, "ok")]
        );

        // A new "start" cycle overrides the old one entirely — no history
        // beyond the current cycle is kept, unlike `logs`.
        db.clone()
            .begin_event_cycle(run_id.clone(), node_id.clone(), "start".to_string())
            .await
            .unwrap();
        db.clone()
            .append_event(
                run_id.clone(),
                node_id.clone(),
                "start".to_string(),
                "resolving config".to_string(),
                "error".to_string(),
                Some("no such file".to_string()),
            )
            .await
            .unwrap();
        let start_events = db
            .clone()
            .list_events(run_id.clone(), node_id.clone(), "start".to_string())
            .await
            .unwrap();
        assert_eq!(start_events.len(), 1);
        assert_eq!(start_events[0].seq, 0);
        assert_eq!(start_events[0].status, "error");
        assert_eq!(start_events[0].detail.as_deref(), Some("no such file"));

        // The unrelated "stop" cycle is still intact.
        let stop_events = db
            .list_events(run_id, node_id, "stop".to_string())
            .await
            .unwrap();
        assert_eq!(stop_events.len(), 1);
    }
}
