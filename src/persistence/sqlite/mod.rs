mod events;
mod logs;
mod runs;
mod schema;

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::Connection;

pub use events::EventEntry;
pub use logs::{LogGeneration, LogLine};

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
        schema::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}
