use anyhow::Result;
use rusqlite::Connection;

pub const CREATE_TABLES: &str = "CREATE TABLE IF NOT EXISTS meta (
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
    CREATE INDEX IF NOT EXISTS idx_events_run_node_action ON events (run_id, node_id, action);";

/// Best-effort migration for `meta`/`containers` databases created before
/// these columns existed — SQLite's `CREATE TABLE IF NOT EXISTS` is a no-op
/// against an already-existing table, so an older db needs these added
/// explicitly. Ignore the error when they're already present (no `IF NOT
/// EXISTS` for `ADD COLUMN` in SQLite).
pub const ADD_COLUMN_MIGRATIONS: &[&str] = &[
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
];

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(CREATE_TABLES)?;
    for stmt in ADD_COLUMN_MIGRATIONS {
        let _ = conn.execute(stmt, []);
    }
    Ok(())
}
