use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::WorkspaceDb;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEntry {
    pub seq: i64,
    pub ts: i64,
    pub step: String,
    pub status: String,
    pub detail: Option<String>,
}

impl WorkspaceDb {
    /// Only the latest cycle of each action is kept.
    pub async fn begin_event_cycle(self: Arc<Self>, node_id: String, action: String) -> Result<()> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "DELETE FROM events WHERE node_id = ?1 AND action = ?2",
                rusqlite::params![node_id, action],
            )?;
            Ok(())
        })
        .await
        .context("begin_event_cycle task panicked")?
    }

    pub async fn append_event(
        self: Arc<Self>,
        node_id: String,
        action: String,
        step: String,
        status: String,
        detail: Option<String>,
    ) -> Result<()> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let next_seq: i64 = conn.query_row(
                "SELECT COALESCE(MAX(seq) + 1, 0) FROM events WHERE node_id = ?1 AND action = ?2",
                rusqlite::params![node_id, action],
                |row| row.get(0),
            )?;
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            conn.execute(
                "INSERT INTO events (node_id, action, seq, ts, step, status, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![node_id, action, next_seq, ts, step, status, detail],
            )?;
            Ok(())
        })
        .await
        .context("append_event task panicked")?
    }

    pub async fn cycle_has_error(self: Arc<Self>, node_id: String, action: String) -> Result<bool> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let found: i64 = conn.query_row(
                "SELECT COUNT(*) FROM events
                 WHERE node_id = ?1 AND action = ?2 AND status = 'error'",
                rusqlite::params![node_id, action],
                |row| row.get(0),
            )?;
            Ok(found > 0)
        })
        .await
        .context("cycle_has_error task panicked")?
    }

    pub async fn list_events(
        self: Arc<Self>,
        node_id: String,
        action: String,
    ) -> Result<Vec<EventEntry>> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT seq, ts, step, status, detail FROM events
                 WHERE node_id = ?1 AND action = ?2
                 ORDER BY seq ASC",
            )?;
            let rows = stmt.query_map(rusqlite::params![node_id, action], |row| {
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
        let node_id = "svc-a".to_string();

        db.clone()
            .begin_event_cycle(node_id.clone(), "start".to_string())
            .await
            .unwrap();
        db.clone()
            .append_event(
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
                node_id.clone(),
                "start".to_string(),
                "resolving config".to_string(),
                "ok".to_string(),
                None,
            )
            .await
            .unwrap();

        db.clone()
            .begin_event_cycle(node_id.clone(), "stop".to_string())
            .await
            .unwrap();
        db.clone()
            .append_event(
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
            .list_events(node_id.clone(), "start".to_string())
            .await
            .unwrap();
        assert_eq!(
            start_events
                .iter()
                .map(|e| (e.seq, e.status.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "running"), (1, "ok")]
        );

        db.clone()
            .begin_event_cycle(node_id.clone(), "start".to_string())
            .await
            .unwrap();
        db.clone()
            .append_event(
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
            .list_events(node_id.clone(), "start".to_string())
            .await
            .unwrap();
        assert_eq!(start_events.len(), 1);
        assert_eq!(start_events[0].seq, 0);
        assert_eq!(start_events[0].status, "error");
        assert_eq!(start_events[0].detail.as_deref(), Some("no such file"));

        let stop_events = db.list_events(node_id, "stop".to_string()).await.unwrap();
        assert_eq!(stop_events.len(), 1);
    }
}
