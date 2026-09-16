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

    if let Some(hint) = hint
        && is_live_socket_for_uid(Path::new(hint))
    {
        return Some(hint.to_string());
    }

    if cfg!(target_os = "macos") {
        let entries = std::fs::read_dir("/private/tmp").ok()?;
        for entry in entries.flatten() {
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with("com.apple.launchd.")
            {
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
    cmd.env(
        "GIT_SSH_COMMAND",
        "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
    );
}

/// `fghjd` owns many workspaces, each of which owns its own durable state
/// under `<workspace>/.fghj/` (see `WorkspaceDb`, below). This file is just
/// the id -> workspace-root pointer list `fghjd` reads on startup to find
/// them all again — restarting the daemon (crash, reboot, upgrade) shouldn't
/// forget which workspaces were wired.
///
/// The durable root `fghjd` owns on this machine — `/var/lib/fghjd` in
/// spirit, but resolved to its real, symlink-free path. On macOS, `/var` is
/// itself a symlink to `/private/var`; bind-mounting a *file* through that
/// symlink (e.g. a workspace's own `.fghj.yaml` mounting
/// `/var/lib/fghjd/ca/bundle.pem`, the one path this daemon documents as
/// stable enough to reference by name) makes OrbStack's mount-type
/// detection misfire with a spurious "not a directory" error, even though
/// both sides of the mount are genuinely regular files. Resolving the
/// symlink ourselves, once, here, means every path built from this root —
/// including the one users write literally into their own `volumes:` — is
/// already immune to it, on every container engine, not just the ones that
/// happen not to trip over the symlink. Linux has no such `/var` symlink, so
/// this is a no-op there.
pub fn fghjd_root() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/private/var/lib/fghjd")
    } else {
        PathBuf::from("/var/lib/fghjd")
    }
}

/// Not `/var/run`: that's commonly a tmpfs wiped on reboot, which would
/// defeat the point of tracking workspaces across a restart. Taken as a
/// parameter (rather than hardcoded) in `load_index`/`save_index` so tests
/// can point it at a tempdir instead of the real root-owned path.
pub fn default_index_path() -> PathBuf {
    fghjd_root().join("workspaces.json")
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
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    std::fs::write(path, serde_json::to_string_pretty(index)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// A small, single-writer bag of daemon-level settings that need to survive
/// `fghjd` restarting on its own (crash, reboot) — today just whether the
/// operator last asked for `daemon stop`, but expected to grow more fields
/// over time (see `DaemonControl` in `daemon.rs`). Plain JSON with
/// `#[serde(default)]` fields, same as `load_index`/`save_index` above:
/// there's only ever one writer and no relational structure here, so a new
/// field is just a new struct field, no migration machinery needed.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    /// Set by `fghj daemon stop`, cleared by `fghj daemon start`. Checked at
    /// `fghjd` startup so a crash/reboot restart comes back idle instead of
    /// silently reactivating behind the operator's back.
    #[serde(default)]
    pub idle_requested: bool,
}

/// Alongside the CA and the workspace index, not `/var/run`, for the same
/// reason as `default_index_path`: this needs to survive a reboot.
pub fn default_state_path() -> PathBuf {
    fghjd_root().join("daemon-state.json")
}

/// A missing or corrupt file just means "defaults" — there's nothing to
/// reconcile against, unlike the workspace index.
pub fn load_daemon_state(path: &Path) -> DaemonState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_daemon_state(path: &Path, state: &DaemonState) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    std::fs::write(path, serde_json::to_string_pretty(state)?)
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
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
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
            );
            CREATE TABLE IF NOT EXISTS logs (
                run_id TEXT NOT NULL,
                node_id TEXT NOT NULL,
                generation INTEGER NOT NULL,
                seq INTEGER NOT NULL,
                stream TEXT NOT NULL,
                ts TEXT NOT NULL,
                line TEXT NOT NULL,
                PRIMARY KEY (run_id, node_id, generation, seq)
            );
            CREATE INDEX IF NOT EXISTS idx_logs_run_node_gen ON logs (run_id, node_id, generation);
            CREATE TABLE IF NOT EXISTS events (
                run_id TEXT NOT NULL,
                node_id TEXT NOT NULL,
                action TEXT NOT NULL,
                seq INTEGER NOT NULL,
                ts INTEGER NOT NULL,
                step TEXT NOT NULL,
                status TEXT NOT NULL,
                detail TEXT,
                PRIMARY KEY (run_id, node_id, action, seq)
            );
            CREATE INDEX IF NOT EXISTS idx_events_run_node_action ON events (run_id, node_id, action);",
        )?;
        // Best-effort migration for `meta`/`containers` databases created
        // before these columns existed — SQLite's `CREATE TABLE IF NOT
        // EXISTS` above is a no-op against an already-existing table, so an
        // older db needs these added explicitly. Ignore the error when
        // they're already present (no `IF NOT EXISTS` for `ADD COLUMN` in
        // SQLite).
        for stmt in [
            "ALTER TABLE meta ADD COLUMN owner_uid INTEGER",
            "ALTER TABLE meta ADD COLUMN owner_gid INTEGER",
            "ALTER TABLE meta ADD COLUMN owner_home TEXT",
            "ALTER TABLE meta ADD COLUMN owner_ssh_auth_sock TEXT",
            "ALTER TABLE containers ADD COLUMN routes_json TEXT",
            "ALTER TABLE containers ADD COLUMN additional_hosts_json TEXT",
            "ALTER TABLE containers ADD COLUMN ports_json TEXT",
            "ALTER TABLE containers ADD COLUMN status_port TEXT",
            "ALTER TABLE containers ADD COLUMN config_hash TEXT",
            "ALTER TABLE containers ADD COLUMN synced INTEGER",
            "ALTER TABLE runs ADD COLUMN sidecar_container_name TEXT",
            "ALTER TABLE runs ADD COLUMN sidecar_ip TEXT",
            "ALTER TABLE containers ADD COLUMN raw_domain TEXT",
        ] {
            let _ = conn.execute(stmt, []);
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
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
                "INSERT INTO runs (run_id, overrides_json, network, sidecar_container_name, sidecar_ip) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(run_id) DO UPDATE SET overrides_json = excluded.overrides_json, network = excluded.network, sidecar_container_name = excluded.sidecar_container_name, sidecar_ip = excluded.sidecar_ip",
                rusqlite::params![
                    state.run_id,
                    serde_json::to_string(&state.overrides)?,
                    state.network,
                    state.sidecar_container_name,
                    state.sidecar_ip,
                ],
            )?;
            tx.execute("DELETE FROM containers WHERE run_id = ?1", rusqlite::params![state.run_id])?;
            for c in &state.containers {
                tx.execute(
                    "INSERT INTO containers (run_id, node_id, container_name, status, published_port, domain, routes_json, additional_hosts_json, ports_json, status_port, config_hash, synced, raw_domain)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    rusqlite::params![
                        state.run_id,
                        c.node_id,
                        c.container_name,
                        c.status,
                        c.published_port,
                        c.domain,
                        serde_json::to_string(&c.routes)?,
                        serde_json::to_string(&c.additional_hosts)?,
                        serde_json::to_string(&c.ports)?,
                        c.status_port,
                        c.config_hash,
                        c.synced,
                        c.raw_domain,
                    ],
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
            conn.execute(
                "DELETE FROM containers WHERE run_id = ?1",
                rusqlite::params![run_id],
            )?;
            conn.execute(
                "DELETE FROM runs WHERE run_id = ?1",
                rusqlite::params![run_id],
            )?;
            Ok(())
        })
        .await
        .context("delete_run task panicked")?
    }

    pub async fn load_runs(self: Arc<Self>) -> Result<BTreeMap<String, RunState>> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let mut runs = BTreeMap::new();
            let mut stmt = conn.prepare(
                "SELECT run_id, overrides_json, network, sidecar_container_name, sidecar_ip FROM runs",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })?;
            for row in rows {
                let (run_id, overrides_json, network, sidecar_container_name, sidecar_ip) = row?;
                let overrides = serde_json::from_str(&overrides_json).unwrap_or_default();
                runs.insert(
                    run_id.clone(),
                    RunState {
                        run_id,
                        overrides,
                        network,
                        containers: Vec::new(),
                        sidecar_container_name: sidecar_container_name.unwrap_or_default(),
                        sidecar_ip,
                    },
                );
            }
            drop(stmt);

            let mut stmt = conn.prepare(
                "SELECT run_id, node_id, container_name, status, published_port, domain, routes_json, additional_hosts_json, ports_json, status_port, config_hash, synced, raw_domain FROM containers",
            )?;
            let rows = stmt.query_map([], |row| {
                let routes_json: Option<String> = row.get(6)?;
                let routes = routes_json
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
                let additional_hosts_json: Option<String> = row.get(7)?;
                let additional_hosts = additional_hosts_json
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
                let ports_json: Option<String> = row.get(8)?;
                let ports = ports_json
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
                Ok((
                    row.get::<_, String>(0)?,
                    ContainerInfo {
                        node_id: row.get(1)?,
                        container_name: row.get(2)?,
                        status: row.get(3)?,
                        published_port: row.get::<_, Option<i64>>(4)?.map(|p| p as u16),
                        domain: row.get(5)?,
                        routes,
                        additional_hosts,
                        ports,
                        status_port: row.get(9)?,
                        config_hash: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
                        synced: row.get::<_, Option<i64>>(11)?.map(|v| v != 0),
                        raw_domain: row.get::<_, Option<String>>(12)?.unwrap_or_default(),
                        pending_action: None,
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

/// One orchestration-level step recorded during a `"start"` or `"stop"`
/// cycle — see [`WorkspaceDb`]'s `events` table. Distinct from [`LogLine`]:
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
        assert_eq!(
            loaded.get("ws-abc"),
            Some(&PathBuf::from("/some/workspace"))
        );
    }

    #[test]
    fn daemon_state_round_trips_and_defaults_to_active() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon-state.json");

        let defaulted = load_daemon_state(&path);
        assert!(
            !defaulted.idle_requested,
            "a freshly-started fghjd with no prior `daemon stop` must default to active"
        );

        save_daemon_state(
            &path,
            &DaemonState {
                idle_requested: true,
            },
        )
        .unwrap();
        assert!(
            load_daemon_state(&path).idle_requested,
            "`daemon stop` must persist so a crash/reboot restart doesn't silently reactivate"
        );

        save_daemon_state(
            &path,
            &DaemonState {
                idle_requested: false,
            },
        )
        .unwrap();
        assert!(
            !load_daemon_state(&path).idle_requested,
            "`daemon start` must clear the flag so future restarts come back active"
        );
    }

    #[tokio::test]
    async fn workspace_db_round_trips_meta_and_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());

        db.clone()
            .record_meta(
                "ws-abc123".to_string(),
                Some("https://example.com/repo.git".to_string()),
            )
            .await
            .unwrap();
        // second call must be a no-op (INSERT OR IGNORE), not an error
        db.clone()
            .record_meta(
                "ws-abc123".to_string(),
                Some("https://example.com/repo.git".to_string()),
            )
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
                routes: vec![crate::runs::PortRoute {
                    domain: "svc-a.demo.fghj".to_string(),
                    host_port: 8080,
                    wildcard: false,
                    https: true,
                    container_port: "8080".to_string(),
                }],
                additional_hosts: Vec::new(),
                ports: BTreeMap::from([("8080".to_string(), Some(8080))]),
                status_port: Some("8080".to_string()),
                config_hash: "deadbeef".to_string(),
                synced: Some(true),
                raw_domain: "svc-a.demo.fghj.raw.internal".to_string(),
                pending_action: None,
            }],
            sidecar_container_name: "fghj-demo-default-sidecar".to_string(),
            sidecar_ip: Some("172.20.0.5".to_string()),
        };
        db.clone().save_run(state).await.unwrap();

        let loaded = db.clone().load_runs().await.unwrap();
        assert_eq!(loaded.len(), 1);
        let restored = &loaded["default"];
        assert_eq!(restored.network, "fghj-demo-default");
        assert_eq!(restored.containers.len(), 1);
        assert_eq!(restored.containers[0].published_port, Some(8080));
        assert_eq!(restored.containers[0].routes.len(), 1);
        assert_eq!(restored.containers[0].routes[0].host_port, 8080);
        assert_eq!(
            restored.overrides.get("svc-a"),
            Some(&"feature-x".to_string())
        );
        assert_eq!(restored.sidecar_container_name, "fghj-demo-default-sidecar");
        assert_eq!(restored.sidecar_ip, Some("172.20.0.5".to_string()));

        db.clone().delete_run("default".to_string()).await.unwrap();
        assert!(db.load_runs().await.unwrap().is_empty());
    }

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
