use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::persistence::workspace_owner::WorkspaceOwner;
use crate::state::{ContainerDesired, ContainerInfo, ContainerObserved, RunState, SyncStatus};

use super::WorkspaceDb;

/// Decodes one of the JSON-blob columns (`routes_json`, `ports_json`,
/// `additional_hosts_json`), falling back to empty for a `NULL` column on a
/// row written before it existed *and* for a blob that no longer parses
/// into the current shape. Both are recoverable the next time the owning
/// container is started through fghj, which rebuilds them from scratch —
/// failing the whole load instead would lose every other run in the db too.
fn json_column<T: serde::de::DeserializeOwned + Default>(raw: Option<String>) -> T {
    raw.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// `synced` is stored as the nullable bool it has always been rather than
/// `SyncStatus`'s names, so an older `fghjd` reading this db still
/// understands the column.
fn sync_to_column(sync: SyncStatus) -> Option<bool> {
    sync.into()
}

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
            for c in state.containers.values() {
                tx.execute(
                    "INSERT INTO containers (run_id, node_id, container_name, status, published_port, domain, routes_json, additional_hosts_json, ports_json, status_port, config_hash, synced, raw_domain, desired_running)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    rusqlite::params![
                        state.run_id,
                        c.node_id,
                        c.desired.container_name,
                        c.observed.status,
                        c.observed.published_port,
                        c.desired.domain,
                        serde_json::to_string(&c.desired.routes)?,
                        serde_json::to_string(&c.desired.additional_hosts)?,
                        serde_json::to_string(&c.observed.ports)?,
                        c.desired.status_port,
                        c.desired.config_hash,
                        sync_to_column(c.observed.sync),
                        c.desired.raw_domain,
                        c.desired.running,
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
                        sidecar_container_name: sidecar_container_name.unwrap_or_default(),
                        sidecar_ip,
                        ..Default::default()
                    },
                );
            }
            drop(stmt);

            let mut stmt = conn.prepare(
                "SELECT run_id, node_id, container_name, status, published_port, domain, routes_json, additional_hosts_json, ports_json, status_port, config_hash, synced, raw_domain, desired_running FROM containers",
            )?;
            let rows = stmt.query_map([], |row| {
                let status: String = row.get(3)?;
                Ok((
                    row.get::<_, String>(0)?,
                    ContainerInfo {
                        node_id: row.get(1)?,
                        desired: ContainerDesired {
                            // A row written before `desired_running`
                            // existed only recorded the outcome, so the
                            // intent behind it has to be inferred from
                            // that one last time. Self-corrects the first
                            // time anything acts on the container.
                            running: row
                                .get::<_, Option<i64>>(13)?
                                .map(|v| v != 0)
                                .unwrap_or(status == "running"),
                            container_name: row.get(2)?,
                            domain: row.get(5)?,
                            raw_domain: row.get::<_, Option<String>>(12)?.unwrap_or_default(),
                            routes: json_column(row.get(6)?),
                            additional_hosts: json_column(row.get(7)?),
                            status_port: row.get(9)?,
                            config_hash: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
                        },
                        observed: ContainerObserved {
                            status,
                            published_port: row.get::<_, Option<i64>>(4)?.map(|p| p as u16),
                            // Never persisted: a container's address on its
                            // Docker network is only meaningful for the
                            // network it is currently attached to.
                            ip: None,
                            ports: json_column(row.get(8)?),
                            sync: row.get::<_, Option<i64>>(11)?.map(|v| v != 0).into(),
                        },
                        // Never a column: an action in flight belongs to
                        // the process that started it, and cannot still be
                        // in flight across a restart.
                        pending_action: None,
                    },
                ))
            })?;
            for row in rows {
                let (run_id, container) = row?;
                if let Some(run) = runs.get_mut(&run_id) {
                    run.containers.insert(container.node_id.clone(), container);
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
    use crate::state::PortRoute;

    fn container() -> ContainerInfo {
        ContainerInfo {
            node_id: "svc-a".to_string(),
            desired: ContainerDesired {
                running: true,
                container_name: "fghj-demo-default-svc-a".to_string(),
                domain: "svc-a.demo.fghj".to_string(),
                raw_domain: "svc-a.demo.fghj.raw.internal".to_string(),
                routes: vec![PortRoute {
                    domain: "svc-a.demo.fghj".to_string(),
                    host_port: 8080,
                    wildcard: false,
                    https: true,
                    container_port: "8080".to_string(),
                }],
                additional_hosts: Vec::new(),
                status_port: Some("8080".to_string()),
                config_hash: "deadbeef".to_string(),
            },
            observed: ContainerObserved {
                status: "running".to_string(),
                published_port: Some(8080),
                ip: None,
                ports: BTreeMap::from([("8080".to_string(), Some(8080))]),
                sync: SyncStatus::Synced,
            },
            pending_action: None,
        }
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
            network: "fghj-demo-default".to_string(),
            containers: BTreeMap::from([("svc-a".to_string(), container())]),
            sidecar_container_name: "fghj-demo-default-sidecar".to_string(),
            sidecar_ip: Some("172.20.0.5".to_string()),
            ..Default::default()
        };
        db.clone().save_run(state.clone()).await.unwrap();

        let loaded = db.clone().load_runs().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["default"], state);

        db.clone().delete_run("default".to_string()).await.unwrap();
        assert!(db.load_runs().await.unwrap().is_empty());
    }

    /// The whole reason `desired` and `observed` are stored in separate
    /// columns: a container fghj wants running but Docker reports as
    /// `exited` has to come back out of the db still saying both of those
    /// things. Round-tripping it through the flat shape this replaced would
    /// have restored it as `desired.running: false` — silently agreeing
    /// with the crash instead of reporting it as drift.
    #[tokio::test]
    async fn a_crashed_container_reloads_still_wanting_to_be_running() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());

        let mut crashed = container();
        crashed.observed.status = "exited".to_string();
        crashed.observed.sync = SyncStatus::Drifted;
        db.clone()
            .save_run(RunState {
                run_id: "default".to_string(),
                containers: BTreeMap::from([("svc-a".to_string(), crashed)]),
                ..Default::default()
            })
            .await
            .unwrap();

        let restored = &db.load_runs().await.unwrap()["default"].containers["svc-a"];
        assert!(restored.desired.running);
        assert_eq!(restored.observed.status, "exited");
        assert_eq!(restored.observed.sync, SyncStatus::Drifted);
    }

    /// A row written by a `fghjd` from before `desired_running` existed has
    /// only the outcome to go on, so intent is inferred from it once.
    #[tokio::test]
    async fn a_row_predating_desired_running_infers_intent_from_status() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        db.clone()
            .save_run(RunState {
                run_id: "default".to_string(),
                containers: BTreeMap::from([("svc-a".to_string(), container())]),
                ..Default::default()
            })
            .await
            .unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute("UPDATE containers SET desired_running = NULL", [])
            .unwrap();

        let restored = &db.load_runs().await.unwrap()["default"].containers["svc-a"];
        assert!(restored.desired.running);
        assert_eq!(restored.observed.status, "running");
    }
}
