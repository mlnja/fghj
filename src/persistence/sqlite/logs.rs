use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::WorkspaceDb;

/// One captured line of container output, tagged with the generation and
/// per-generation sequence number it belongs to (see [`WorkspaceDb`]'s
/// `logs` table and [`WorkspaceDb::begin_log_generation`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub seq: i64,
    pub stream: String,
    pub ts: String,
    pub line: String,
}

/// Summary of one retained log generation, for the UI's generation picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogGeneration {
    pub generation: i64,
    pub line_count: i64,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
}

impl WorkspaceDb {
    /// Starts a new log generation for `(run_id, node_id)`, returning the
    /// generation number the caller should tag every line it captures from
    /// here on. Generations are derived live from the table itself (rather
    /// than tracked in a separate counter) so they stay monotonically
    /// increasing even as old ones are pruned below.
    ///
    /// Retention: only the new generation and the one immediately before it
    /// are kept — anything older is deleted here, at the moment a new
    /// generation begins, per the explicit "discard logs more than one
    /// container old" requirement. This bounds growth without needing a
    /// separate cleanup pass.
    pub async fn begin_log_generation(
        self: Arc<Self>,
        run_id: String,
        node_id: String,
    ) -> Result<i64> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let prev_max: Option<i64> = conn.query_row(
                "SELECT MAX(generation) FROM logs WHERE run_id = ?1 AND node_id = ?2",
                rusqlite::params![run_id, node_id],
                |row| row.get(0),
            )?;
            let generation = prev_max.map(|g| g + 1).unwrap_or(0);
            conn.execute(
                "DELETE FROM logs WHERE run_id = ?1 AND node_id = ?2 AND generation <= ?3",
                rusqlite::params![run_id, node_id, generation - 2],
            )?;
            Ok(generation)
        })
        .await
        .context("begin_log_generation task panicked")?
    }

    /// Appends captured lines for a generation already opened by
    /// [`Self::begin_log_generation`]. Batched into one transaction per call
    /// by the caller (which buffers lines before flushing) rather than one
    /// transaction per line.
    pub async fn insert_log_lines(
        self: Arc<Self>,
        run_id: String,
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
                    "INSERT OR IGNORE INTO logs (run_id, node_id, generation, seq, stream, ts, line)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        run_id,
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

    /// Lists the generations retained for `(run_id, node_id)`, most recent
    /// first, so the UI can offer a "current" vs. "previous (crashed?)"
    /// picker.
    pub async fn list_log_generations(
        self: Arc<Self>,
        run_id: String,
        node_id: String,
    ) -> Result<Vec<LogGeneration>> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT generation, COUNT(*), MIN(ts), MAX(ts) FROM logs
                 WHERE run_id = ?1 AND node_id = ?2
                 GROUP BY generation ORDER BY generation DESC",
            )?;
            let rows = stmt.query_map(rusqlite::params![run_id, node_id], |row| {
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

    /// Loads up to `limit` lines from `generation`, older than `before_seq`
    /// (or the most recent `limit` lines, if `before_seq` is `None`) —
    /// backing both the initial load and "scroll up for more history" in the
    /// UI. Always returned oldest-first, regardless of scan direction.
    pub async fn load_log_history(
        self: Arc<Self>,
        run_id: String,
        node_id: String,
        generation: i64,
        before_seq: Option<i64>,
        limit: i64,
    ) -> Result<Vec<LogLine>> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT seq, stream, ts, line FROM logs
                 WHERE run_id = ?1 AND node_id = ?2 AND generation = ?3
                   AND (?4 IS NULL OR seq < ?4)
                 ORDER BY seq DESC LIMIT ?5",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![run_id, node_id, generation, before_seq, limit],
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
        let run_id = "default".to_string();
        let node_id = "svc-a".to_string();

        let line = |seq: i64, text: &str| LogLine {
            seq,
            stream: "stdout".to_string(),
            ts: "2026-01-01T00:00:00Z".to_string(),
            line: text.to_string(),
        };

        // Generation 0.
        let gen0 = db
            .clone()
            .begin_log_generation(run_id.clone(), node_id.clone())
            .await
            .unwrap();
        assert_eq!(gen0, 0);
        db.clone()
            .insert_log_lines(run_id.clone(), node_id.clone(), gen0, vec![line(0, "boot")])
            .await
            .unwrap();

        // Generation 1 — still within the retention window, gen0 survives.
        let gen1 = db
            .clone()
            .begin_log_generation(run_id.clone(), node_id.clone())
            .await
            .unwrap();
        assert_eq!(gen1, 1);
        db.clone()
            .insert_log_lines(
                run_id.clone(),
                node_id.clone(),
                gen1,
                vec![line(0, "crash")],
            )
            .await
            .unwrap();

        let generations = db
            .clone()
            .list_log_generations(run_id.clone(), node_id.clone())
            .await
            .unwrap();
        assert_eq!(
            generations.iter().map(|g| g.generation).collect::<Vec<_>>(),
            vec![1, 0]
        );

        // Generation 2 pushes gen0 out of the two-generation retention
        // window — only 1 and 2 should remain.
        let gen2 = db
            .clone()
            .begin_log_generation(run_id.clone(), node_id.clone())
            .await
            .unwrap();
        assert_eq!(gen2, 2);
        db.clone()
            .insert_log_lines(run_id.clone(), node_id.clone(), gen2, vec![line(0, "ok")])
            .await
            .unwrap();

        let generations = db
            .clone()
            .list_log_generations(run_id.clone(), node_id.clone())
            .await
            .unwrap();
        assert_eq!(
            generations.iter().map(|g| g.generation).collect::<Vec<_>>(),
            vec![2, 1]
        );

        // Pagination: the crash generation's lines are still fully readable.
        db.clone()
            .insert_log_lines(
                run_id.clone(),
                node_id.clone(),
                gen1,
                vec![line(1, "stack trace")],
            )
            .await
            .unwrap();
        let history = db
            .clone()
            .load_log_history(run_id.clone(), node_id.clone(), gen1, None, 10)
            .await
            .unwrap();
        assert_eq!(
            history.iter().map(|l| l.line.as_str()).collect::<Vec<_>>(),
            vec!["crash", "stack trace"]
        );
        let older = db
            .load_log_history(run_id, node_id, gen1, Some(1), 10)
            .await
            .unwrap();
        assert_eq!(
            older.iter().map(|l| l.line.as_str()).collect::<Vec<_>>(),
            vec!["crash"]
        );
    }
}
