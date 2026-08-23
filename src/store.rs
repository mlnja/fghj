use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::runs::{ContainerInfo, RunState};

/// `fghjd` owns many workspaces, each of which owns its own durable state
/// under `<workspace>/.fghj/` (see `WorkspaceDb`, below). This file is just
/// the id -> workspace-root pointer list `fghjd` reads on startup to find
/// them all again — restarting the daemon (crash, reboot, upgrade) shouldn't
/// forget which workspaces were wired.
///
/// Not `/var/run`: that's commonly a tmpfs wiped on reboot, which would
/// defeat the point of tracking workspaces across a restart. Taken as a
/// parameter (rather than hardcoded) in `load_index`/`save_index` so tests
/// can point it at a tempdir instead of the real root-owned path.
pub fn default_index_path() -> PathBuf {
    PathBuf::from("/var/lib/fghjd/workspaces.json")
}

/// A missing or corrupt index just means "no workspaces known yet", not a
/// startup failure — every entry is independently re-verified against disk
/// by `WorkspaceRegistry::load` anyway.
pub fn load_index(path: &Path) -> HashMap<String, PathBuf> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_index(path: &Path, index: &HashMap<String, PathBuf>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }
    std::fs::write(path, serde_json::to_string_pretty(index)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Per-workspace SQLite store at `<workspace>/.fghj/fghj.db` — the durable
/// twin of `RunRegistry`'s in-memory state, colocated with the workspace
/// (like `.git`) so it travels with the checkout rather than living only in
/// `fghjd`'s process memory. Reopened and reconciled against real docker
/// state on every daemon startup by `RunRegistry::new`.
pub struct WorkspaceDb {
    conn: Mutex<Connection>,
}

impl WorkspaceDb {
    pub fn open(workspace: &Path) -> Result<Self> {
        let dir = workspace.join(".fghj");
        std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let db_path = dir.join("fghj.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open {}", db_path.display()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                id TEXT PRIMARY KEY,
                entry TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runs (
                run_id TEXT PRIMARY KEY,
                overrides_json TEXT NOT NULL,
                network TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS containers (
                run_id TEXT NOT NULL,
                node_id TEXT NOT NULL,
                container_name TEXT NOT NULL,
                status TEXT NOT NULL,
                published_port INTEGER,
                domain TEXT NOT NULL,
                PRIMARY KEY (run_id, node_id)
            );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Records the workspace's identity the first time it's wired; a no-op
    /// on every later `wire` of the same path.
    pub fn record_meta(&self, id: &str, entry: Option<&str>) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.conn.lock().unwrap().execute(
            "INSERT OR IGNORE INTO meta (id, entry, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, entry, now.to_string()],
        )?;
        Ok(())
    }

    pub fn save_run(&self, state: &RunState) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO runs (run_id, overrides_json, network) VALUES (?1, ?2, ?3)
             ON CONFLICT(run_id) DO UPDATE SET overrides_json = excluded.overrides_json, network = excluded.network",
            rusqlite::params![state.run_id, serde_json::to_string(&state.overrides)?, state.network],
        )?;
        tx.execute("DELETE FROM containers WHERE run_id = ?1", rusqlite::params![state.run_id])?;
        for c in &state.containers {
            tx.execute(
                "INSERT INTO containers (run_id, node_id, container_name, status, published_port, domain)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![state.run_id, c.node_id, c.container_name, c.status, c.published_port, c.domain],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_run(&self, run_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM containers WHERE run_id = ?1", rusqlite::params![run_id])?;
        conn.execute("DELETE FROM runs WHERE run_id = ?1", rusqlite::params![run_id])?;
        Ok(())
    }

    pub fn load_runs(&self) -> Result<BTreeMap<String, RunState>> {
        let conn = self.conn.lock().unwrap();
        let mut runs = BTreeMap::new();
        let mut stmt = conn.prepare("SELECT run_id, overrides_json, network FROM runs")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;
        for row in rows {
            let (run_id, overrides_json, network) = row?;
            let overrides = serde_json::from_str(&overrides_json).unwrap_or_default();
            runs.insert(run_id.clone(), RunState { run_id, overrides, network, containers: Vec::new() });
        }
        drop(stmt);

        let mut stmt = conn.prepare(
            "SELECT run_id, node_id, container_name, status, published_port, domain FROM containers",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                ContainerInfo {
                    node_id: row.get(1)?,
                    container_name: row.get(2)?,
                    status: row.get(3)?,
                    published_port: row.get::<_, Option<i64>>(4)?.map(|p| p as u16),
                    domain: row.get(5)?,
                },
            ))
        })?;
        for row in rows {
            let (run_id, container) = row?;
            if let Some(run) = runs.get_mut(&run_id) {
                run.containers.push(container);
            }
        }
        Ok(runs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn index_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workspaces.json");

        assert!(load_index(&path).is_empty());

        let mut index = HashMap::new();
        index.insert("ws-abc".to_string(), PathBuf::from("/some/workspace"));
        save_index(&path, &index).unwrap();

        let loaded = load_index(&path);
        assert_eq!(loaded.get("ws-abc"), Some(&PathBuf::from("/some/workspace")));
    }

    #[test]
    fn workspace_db_round_trips_meta_and_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let db = WorkspaceDb::open(tmp.path()).unwrap();

        db.record_meta("ws-abc123", Some("https://example.com/repo.git")).unwrap();
        // second call must be a no-op (INSERT OR IGNORE), not an error
        db.record_meta("ws-abc123", Some("https://example.com/repo.git")).unwrap();

        let state = RunState {
            run_id: "default".to_string(),
            overrides: BTreeMap::from([("svc-a".to_string(), "feature-x".to_string())]),
            network: "fghj-demo-default".to_string(),
            containers: vec![ContainerInfo {
                node_id: "svc-a".to_string(),
                container_name: "fghj-demo-default-svc-a".to_string(),
                status: "running".to_string(),
                published_port: Some(8080),
                domain: "svc-a.demo.fghj".to_string(),
            }],
        };
        db.save_run(&state).unwrap();

        let loaded = db.load_runs().unwrap();
        assert_eq!(loaded.len(), 1);
        let restored = &loaded["default"];
        assert_eq!(restored.network, "fghj-demo-default");
        assert_eq!(restored.containers.len(), 1);
        assert_eq!(restored.containers[0].published_port, Some(8080));
        assert_eq!(restored.overrides.get("svc-a"), Some(&"feature-x".to_string()));

        db.delete_run("default").unwrap();
        assert!(db.load_runs().unwrap().is_empty());
    }
}
