use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::WorkspaceDb;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub seq: i64,
    pub stream: String,
    pub ts: String,
    pub line: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogGeneration {
    pub generation: i64,
    pub line_count: i64,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
}

impl WorkspaceDb {
    /// Returns the new generation number. Keeps only it and the one before.
    pub async fn begin_log_generation(self: Arc<Self>, node_id: String) -> Result<i64> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let prev_max: Option<i64> = conn.query_row(
                "SELECT MAX(generation) FROM logs WHERE node_id = ?1",
                rusqlite::params![node_id],
                |row| row.get(0),
            )?;
            let generation = prev_max.map(|g| g + 1).unwrap_or(0);
            conn.execute(
                "DELETE FROM logs WHERE node_id = ?1 AND generation <= ?2",
                rusqlite::params![node_id, generation - 2],
            )?;
            Ok(generation)
        })
        .await
        .context("begin_log_generation task panicked")?
    }

    pub async fn insert_log_lines(
        self: Arc<Self>,
        node_id: String,
        generation: i64,
        lines: Vec<LogLine>,
    ) -> Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        tokio::task::spawn_blocking(move || {
            let mut conn = self.conn.lock().unwrap();
            let tx = conn.transaction()?;
            for line in &lines {
                tx.execute(
                    "INSERT OR IGNORE INTO logs (node_id, generation, seq, stream, ts, line)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        node_id,
                        generation,
                        line.seq,
                        line.stream,
                        line.ts,
                        line.line,
                    ],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .context("insert_log_lines task panicked")?
    }

    pub async fn list_log_generations(
        self: Arc<Self>,
        node_id: String,
    ) -> Result<Vec<LogGeneration>> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT generation, COUNT(*), MIN(ts), MAX(ts) FROM logs
                 WHERE node_id = ?1
                 GROUP BY generation ORDER BY generation DESC",
            )?;
            let rows = stmt.query_map(rusqlite::params![node_id], |row| {
                Ok(LogGeneration {
                    generation: row.get(0)?,
                    line_count: row.get(1)?,
                    first_ts: row.get(2)?,
                    last_ts: row.get(3)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into)
        })
        .await
        .context("list_log_generations task panicked")?
    }

    /// Lines older than `before_seq` (or the latest), returned oldest-first.
    pub async fn load_log_history(
        self: Arc<Self>,
        node_id: String,
        generation: i64,
        before_seq: Option<i64>,
        limit: i64,
    ) -> Result<Vec<LogLine>> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT seq, stream, ts, line FROM logs
                 WHERE node_id = ?1 AND generation = ?2
                   AND (?3 IS NULL OR seq < ?3)
                 ORDER BY seq DESC LIMIT ?4",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![node_id, generation, before_seq, limit],
                |row| {
                    Ok(LogLine {
                        seq: row.get(0)?,
                        stream: row.get(1)?,
                        ts: row.get(2)?,
                        line: row.get(3)?,
                    })
                },
            )?;
            let mut lines = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            lines.reverse();
            Ok(lines)
        })
        .await
        .context("load_log_history task panicked")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn log_generations_are_numbered_and_pruned_to_the_last_two() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        let node_id = "svc-a".to_string();

        let line = |seq: i64, text: &str| LogLine {
            seq,
            stream: "stdout".to_string(),
            ts: "2026-01-01T00:00:00Z".to_string(),
            line: text.to_string(),
        };

        let gen0 = db
            .clone()
            .begin_log_generation(node_id.clone())
            .await
            .unwrap();
        assert_eq!(gen0, 0);
        db.clone()
            .insert_log_lines(node_id.clone(), gen0, vec![line(0, "boot")])
            .await
            .unwrap();

        let gen1 = db
            .clone()
            .begin_log_generation(node_id.clone())
            .await
            .unwrap();
        assert_eq!(gen1, 1);
        db.clone()
            .insert_log_lines(node_id.clone(), gen1, vec![line(0, "crash")])
            .await
            .unwrap();

        let generations = db
            .clone()
            .list_log_generations(node_id.clone())
            .await
            .unwrap();
        assert_eq!(
            generations.iter().map(|g| g.generation).collect::<Vec<_>>(),
            vec![1, 0]
        );

        let gen2 = db
            .clone()
            .begin_log_generation(node_id.clone())
            .await
            .unwrap();
        assert_eq!(gen2, 2);
        db.clone()
            .insert_log_lines(node_id.clone(), gen2, vec![line(0, "ok")])
            .await
            .unwrap();

        let generations = db
            .clone()
            .list_log_generations(node_id.clone())
            .await
            .unwrap();
        assert_eq!(
            generations.iter().map(|g| g.generation).collect::<Vec<_>>(),
            vec![2, 1]
        );

        db.clone()
            .insert_log_lines(node_id.clone(), gen1, vec![line(1, "stack trace")])
            .await
            .unwrap();
        let history = db
            .clone()
            .load_log_history(node_id.clone(), gen1, None, 10)
            .await
            .unwrap();
        assert_eq!(
            history.iter().map(|l| l.line.as_str()).collect::<Vec<_>>(),
            vec!["crash", "stack trace"]
        );
        let older = db
            .load_log_history(node_id, gen1, Some(1), 10)
            .await
            .unwrap();
        assert_eq!(
            older.iter().map(|l| l.line.as_str()).collect::<Vec<_>>(),
            vec!["crash"]
        );
    }
}
