mod env;
mod events;
mod logs;
mod schema;
mod seeds;

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::Connection;

pub use events::EventEntry;
pub use logs::{LogGeneration, LogLine};

/// `<workspace>/.fghj/fghj.db`, so state travels with the checkout.
pub struct WorkspaceDb {
    conn: Mutex<Connection>,
}

impl WorkspaceDb {
    pub fn open(workspace: &Path) -> Result<Self> {
        let dir = workspace.join(".fghj");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        let db_path = dir.join("fghj.db");
        let mut conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open {}", db_path.display()))?;
        schema::init(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}
