use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::runs::{ContainerInfo, RunState};

/// The real user who ran `fghj wire`, captured at registration time by the
/// unprivileged `fghj` CLI (which has the correct uid/env) and persisted so
/// `fghjd` — running as root — can later drop privileges back to this user
/// before shelling out to `git clone` against a remote the daemon itself has
/// no credentials for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceOwner {
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub ssh_auth_sock: Option<String>,
}

impl WorkspaceOwner {
    /// Configures `cmd` to run as this user instead of whoever spawned it
    /// (`fghjd`, running as root) — root bypasses the file permission check
    /// on this user's ssh-agent socket, so it can use their credentials even
    /// though it isn't them, provided it knows where to look (`HOME`,
    /// `SSH_AUTH_SOCK`).
    pub fn apply_to_command(&self, cmd: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        cmd.uid(self.uid).gid(self.gid).env("HOME", &self.home);
        match live_ssh_auth_sock(self.uid, self.ssh_auth_sock.as_deref()) {
            Some(sock) => {
                cmd.env("SSH_AUTH_SOCK", sock);
            }
            None => {
                cmd.env_remove("SSH_AUTH_SOCK");
            }
        }
    }
}

/// Finds a live ssh-agent socket for `uid`, rather than trusting `hint` (the
/// `SSH_AUTH_SOCK` captured from one shell's environment at `fghj wire`
/// time) blindly forever. `hint` can go stale — the agent that created it may
/// have been restarted — and re-deriving it fresh is what makes this actually
/// track the user rather than a snapshot of one of their terminal sessions.
///
/// macOS's system agent is bound to the *login* session (via `launchd`), not
/// any one terminal, and lives at a deterministic, discoverable path, so a
/// dead `hint` can be recovered by scanning for it; on other platforms there
/// is no equivalent well-known path, so a dead hint is simply unusable.
fn live_ssh_auth_sock(uid: u32, hint: Option<&str>) -> Option<String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let is_live_socket_for_uid = |path: &Path| -> bool {
        std::fs::metadata(path)
            .map(|m| m.uid() == uid && m.file_type().is_socket())
            .unwrap_or(false)
    };

    if let Some(hint) = hint {
        if is_live_socket_for_uid(Path::new(hint)) {
            return Some(hint.to_string());
        }
    }

    if cfg!(target_os = "macos") {
        let entries = std::fs::read_dir("/private/tmp").ok()?;
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("com.apple.launchd.") {
                continue;
            }
            let candidate = entry.path().join("Listeners");
            if is_live_socket_for_uid(&candidate) {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }

    None
}

/// Disables all interactive ssh prompting (`BatchMode`) and auto-accepts
/// unknown host keys, so a `git clone` subprocess either succeeds or fails
/// fast and visibly instead of hanging forever on a host-key prompt that's
/// written straight to the parent process's controlling tty — invisible to
/// (and unanswerable from) anything capturing its piped stdout/stderr.
/// Worth applying even when also running as the real owner via
/// [`WorkspaceOwner::apply_to_command`], as a second line of defense.
pub fn harden_git_ssh(cmd: &mut std::process::Command) {
    cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new");
}

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
                created_at TEXT NOT NULL,
                owner_uid INTEGER,
                owner_gid INTEGER,
                owner_home TEXT,
                owner_ssh_auth_sock TEXT
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
        // Best-effort migration for `meta` databases created before the
        // owner_* columns existed — SQLite's `CREATE TABLE IF NOT EXISTS`
        // above is a no-op against an already-existing table, so an older db
        // needs these added explicitly. Ignore the error when they're
        // already present (no `IF NOT EXISTS` for `ADD COLUMN` in SQLite).
        for stmt in [
            "ALTER TABLE meta ADD COLUMN owner_uid INTEGER",
            "ALTER TABLE meta ADD COLUMN owner_gid INTEGER",
            "ALTER TABLE meta ADD COLUMN owner_home TEXT",
            "ALTER TABLE meta ADD COLUMN owner_ssh_auth_sock TEXT",
        ] {
            let _ = conn.execute(stmt, []);
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Records the workspace's identity the first time it's wired; a no-op
    /// on every later `wire` of the same path.
    pub async fn record_meta(self: Arc<Self>, id: String, entry: Option<String>) -> Result<()> {
        tokio::task::spawn_blocking(move || {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            self.conn.lock().unwrap().execute(
                "INSERT OR IGNORE INTO meta (id, entry, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, entry, now.to_string()],
            )?;
            Ok(())
        })
        .await
        .context("record_meta task panicked")?
    }

    /// Refreshed on every `fghj wire` of this workspace (not just the
    /// first), since the captured `ssh_auth_sock` is only valid for the
    /// login session that was live when it was sent.
    pub async fn set_owner(self: Arc<Self>, id: String, owner: WorkspaceOwner) -> Result<()> {
        tokio::task::spawn_blocking(move || {
            self.conn.lock().unwrap().execute(
                "UPDATE meta SET owner_uid = ?1, owner_gid = ?2, owner_home = ?3, owner_ssh_auth_sock = ?4 WHERE id = ?5",
                rusqlite::params![owner.uid, owner.gid, owner.home, owner.ssh_auth_sock, id],
            )?;
            Ok(())
        })
        .await
        .context("set_owner task panicked")?
    }

    /// Each db is colocated with exactly one workspace, so there is at most
    /// one `meta` row — no need to match by id.
    pub async fn load_owner(self: Arc<Self>) -> Result<Option<WorkspaceOwner>> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let result = conn.query_row(
                "SELECT owner_uid, owner_gid, owner_home, owner_ssh_auth_sock FROM meta WHERE owner_uid IS NOT NULL LIMIT 1",
                [],
                |row| {
                    Ok(WorkspaceOwner {
                        uid: row.get(0)?,
                        gid: row.get(1)?,
                        home: row.get(2)?,
                        ssh_auth_sock: row.get(3)?,
                    })
                },
            );
            match result {
                Ok(owner) => Ok(Some(owner)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await
        .context("load_owner task panicked")?
    }

    pub async fn save_run(self: Arc<Self>, state: RunState) -> Result<()> {
        tokio::task::spawn_blocking(move || {
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
        })
        .await
        .context("save_run task panicked")?
    }

    pub async fn delete_run(self: Arc<Self>, run_id: String) -> Result<()> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            conn.execute("DELETE FROM containers WHERE run_id = ?1", rusqlite::params![run_id])?;
            conn.execute("DELETE FROM runs WHERE run_id = ?1", rusqlite::params![run_id])?;
            Ok(())
        })
        .await
        .context("delete_run task panicked")?
    }

    pub async fn load_runs(self: Arc<Self>) -> Result<BTreeMap<String, RunState>> {
        tokio::task::spawn_blocking(move || {
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
        })
        .await
        .context("load_runs task panicked")?
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

    #[tokio::test]
    async fn workspace_db_round_trips_meta_and_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());

        db.clone()
            .record_meta("ws-abc123".to_string(), Some("https://example.com/repo.git".to_string()))
            .await
            .unwrap();
        // second call must be a no-op (INSERT OR IGNORE), not an error
        db.clone()
            .record_meta("ws-abc123".to_string(), Some("https://example.com/repo.git".to_string()))
            .await
            .unwrap();

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
        db.clone().save_run(state).await.unwrap();

        let loaded = db.clone().load_runs().await.unwrap();
        assert_eq!(loaded.len(), 1);
        let restored = &loaded["default"];
        assert_eq!(restored.network, "fghj-demo-default");
        assert_eq!(restored.containers.len(), 1);
        assert_eq!(restored.containers[0].published_port, Some(8080));
        assert_eq!(restored.overrides.get("svc-a"), Some(&"feature-x".to_string()));

        db.clone().delete_run("default".to_string()).await.unwrap();
        assert!(db.load_runs().await.unwrap().is_empty());
    }
}
