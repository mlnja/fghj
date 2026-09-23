use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::persistence::workspace_owner::WorkspaceOwner;
use crate::runs::{ContainerInfo, RunState};

use super::WorkspaceDb;

impl WorkspaceDb {
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
            // `overrides_json` is a leftover NOT NULL column from the now-removed
            // branch-override feature — still written as an empty object so
            // inserts keep satisfying the constraint on pre-existing databases
            // that still have the column, but no longer read back into anything.
            tx.execute(
                "INSERT INTO runs (run_id, overrides_json, network, sidecar_container_name, sidecar_ip) VALUES (?1, '{}', ?2, ?3, ?4)
                 ON CONFLICT(run_id) DO UPDATE SET network = excluded.network, sidecar_container_name = excluded.sidecar_container_name, sidecar_ip = excluded.sidecar_ip",
                rusqlite::params![
                    state.run_id,
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
                "SELECT run_id, network, sidecar_container_name, sidecar_ip FROM runs",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            for row in rows {
                let (run_id, network, sidecar_container_name, sidecar_ip) = row?;
                runs.insert(
                    run_id.clone(),
                    RunState {
                        run_id,
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(restored.sidecar_container_name, "fghj-demo-default-sidecar");
        assert_eq!(restored.sidecar_ip, Some("172.20.0.5".to_string()));

        db.clone().delete_run("default".to_string()).await.unwrap();
        assert!(db.load_runs().await.unwrap().is_empty());
    }
}
