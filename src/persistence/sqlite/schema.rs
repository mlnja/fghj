use anyhow::Result;
use rusqlite::Connection;

const VERSION: i64 = 3;

const CREATE_TABLES: &str = "
    CREATE TABLE owner (
        id INTEGER PRIMARY KEY CHECK (id = 0),
        uid INTEGER NOT NULL,
        gid INTEGER NOT NULL,
        home TEXT NOT NULL,
        ssh_auth_sock TEXT
    );
    CREATE TABLE env (
        id INTEGER PRIMARY KEY CHECK (id = 0),
        network TEXT NOT NULL,
        sidecar_container_name TEXT NOT NULL,
        sidecar_ip TEXT
    );
    CREATE TABLE containers (
        node_id TEXT PRIMARY KEY,
        desired TEXT NOT NULL,
        observed TEXT NOT NULL
    );
    CREATE TABLE logs (
        node_id TEXT NOT NULL,
        generation INTEGER NOT NULL,
        seq INTEGER NOT NULL,
        stream TEXT NOT NULL,
        ts TEXT NOT NULL,
        line TEXT NOT NULL,
        PRIMARY KEY (node_id, generation, seq)
    );
    CREATE TABLE events (
        node_id TEXT NOT NULL,
        action TEXT NOT NULL,
        seq INTEGER NOT NULL,
        ts INTEGER NOT NULL,
        step TEXT NOT NULL,
        status TEXT NOT NULL,
        detail TEXT,
        PRIMARY KEY (node_id, action, seq)
    );";

pub fn init(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == VERSION {
        return Ok(());
    }
    let tx = conn.transaction()?;
    let tables: Vec<String> = tx
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for table in tables {
        tx.execute_batch(&format!("DROP TABLE \"{table}\""))?;
    }
    tx.execute_batch(CREATE_TABLES)?;
    tx.execute_batch(&format!("PRAGMA user_version = {VERSION}"))?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_database_at_another_version_is_recreated() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runs (run_id TEXT PRIMARY KEY); INSERT INTO runs VALUES ('default');",
        )
        .unwrap();

        init(&mut conn).unwrap();

        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'runs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0);
        conn.execute(
            "INSERT INTO env (id, network, sidecar_container_name) VALUES (0, 'n', 's')",
            [],
        )
        .unwrap();
        init(&mut conn).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM env", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }
}
