use std::sync::Arc;

use anyhow::{Context, Result};

use super::WorkspaceDb;

/// The data a seed last succeeded against, and when.
pub struct SeededData {
    pub data: String,
    /// Unix seconds.
    pub at: i64,
}

impl WorkspaceDb {
    pub async fn load_seed(self: Arc<Self>, node_id: String) -> Result<Option<SeededData>> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let result = conn.query_row(
                "SELECT data, at FROM seeds WHERE node_id = ?1",
                [&node_id],
                |row| {
                    Ok(SeededData {
                        data: row.get(0)?,
                        at: row.get(1)?,
                    })
                },
            );
            match result {
                Ok(seeded) => Ok(Some(seeded)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await
        .context("load_seed task panicked")?
    }

    pub async fn save_seed(self: Arc<Self>, node_id: String, data: String) -> Result<()> {
        tokio::task::spawn_blocking(move || {
            let at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            self.conn.lock().unwrap().execute(
                "INSERT OR REPLACE INTO seeds (node_id, data, at) VALUES (?1, ?2, ?3)",
                rusqlite::params![node_id, data, at],
            )?;
            Ok(())
        })
        .await
        .context("save_seed task panicked")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_seed_record_is_kept_per_node_and_replaced_by_the_next() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        assert!(
            db.clone()
                .load_seed("seed.api".into())
                .await
                .unwrap()
                .is_none()
        );

        db.clone()
            .save_seed("seed.api".into(), "a".into())
            .await
            .unwrap();
        db.clone()
            .save_seed("seed.api".into(), "b".into())
            .await
            .unwrap();
        let seeded = db
            .clone()
            .load_seed("seed.api".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seeded.data, "b");
        assert!(db.load_seed("other.api".into()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reopening_keeps_what_was_seeded() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        db.save_seed("seed.api".into(), "a".into()).await.unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        assert_eq!(
            db.load_seed("seed.api".into()).await.unwrap().unwrap().data,
            "a"
        );
    }
}
