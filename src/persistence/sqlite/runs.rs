#[cfg(test)]
use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::persistence::workspace_owner::WorkspaceOwner;
use crate::runs::DEFAULT_RUN_ID;
use crate::state::{ContainerDesired, ContainerInfo, ContainerObserved, EnvState, SyncStatus};

use super::WorkspaceDb;

/// Decodes one of the JSON-blob columns (`routes_json`, `ports_json`,
/// `additional_hosts_json`), falling back to empty for a `NULL` column on a
/// row written before it existed *and* for a blob that no longer parses
/// into the current shape. Both are recoverable the next time the owning
/// container is started through fghj, which rebuilds them from scratch —
/// failing the whole load instead would lose every other container too.
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

    pub async fn save_env(self: Arc<Self>, state: EnvState) -> Result<()> {
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
                    DEFAULT_RUN_ID,
                    state.network,
                    state.sidecar_container_name,
                    state.sidecar_ip,
                ],
            )?;
            tx.execute("DELETE FROM containers WHERE run_id = ?1", rusqlite::params![DEFAULT_RUN_ID])?;
            for c in state.containers.values() {
                tx.execute(
                    "INSERT INTO containers (run_id, node_id, container_name, status, published_port, domain, routes_json, additional_hosts_json, ports_json, status_port, config_hash, synced, raw_domain, desired_running, terminating, exit_code, debug_wait, source_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                    rusqlite::params![
                        DEFAULT_RUN_ID,
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
                        c.desired.terminating,
                        c.observed.exit_code,
                        c.desired.debug_wait,
                        serde_json::to_string(&c.desired.source)?,
                    ],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .context("save_env task panicked")?
    }

    pub async fn delete_env(self: Arc<Self>) -> Result<()> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "DELETE FROM containers WHERE run_id = ?1",
                rusqlite::params![DEFAULT_RUN_ID],
            )?;
            conn.execute(
                "DELETE FROM runs WHERE run_id = ?1",
                rusqlite::params![DEFAULT_RUN_ID],
            )?;
            Ok(())
        })
        .await
        .context("delete_env task panicked")?
    }

    pub async fn load_env(self: Arc<Self>) -> Result<Option<EnvState>> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let env = conn.query_row(
                "SELECT network, sidecar_container_name, sidecar_ip FROM runs WHERE run_id = ?1",
                rusqlite::params![DEFAULT_RUN_ID],
                |row| {
                    Ok(EnvState {
                        network: row.get(0)?,
                        sidecar_container_name: row
                            .get::<_, Option<String>>(1)?
                            .unwrap_or_default(),
                        sidecar_ip: row.get(2)?,
                        ..Default::default()
                    })
                },
            );
            let mut env = match env {
                Ok(env) => env,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
                Err(e) => return Err(e.into()),
            };

            let mut stmt = conn.prepare(
                "SELECT run_id, node_id, container_name, status, published_port, domain, routes_json, additional_hosts_json, ports_json, status_port, config_hash, synced, raw_domain, desired_running, terminating, exit_code, debug_wait, source_json FROM containers WHERE run_id = ?1",
            )?;
            let rows = stmt.query_map(rusqlite::params![DEFAULT_RUN_ID], |row| {
                let status: String = row.get(3)?;
                Ok(
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
                            // A row written before this column existed
                            // predates the task kind entirely, so there is
                            // nothing to guess: it can only have been a
                            // service.
                            terminating: row.get::<_, Option<i64>>(14)?.is_some_and(|v| v != 0),
                            // Absent in a row written before the column
                            // existed, which means off — the default, and
                            // the only safe guess: inferring "halt at
                            // startup" would block the node on a human who
                            // isn't there.
                            debug_wait: row.get::<_, Option<i64>>(16)?.is_some_and(|v| v != 0),
                            source: json_column(row.get(17)?),
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
                            exit_code: row.get(15)?,
                        },
                        // Never a column: an action in flight belongs to
                        // the process that started it, and cannot still be
                        // in flight across a restart.
                        pending_action: None,
                    },
                )
            })?;
            for row in rows {
                let container = row?;
                env.containers.insert(container.node_id.clone(), container);
            }
            drop(stmt);
            Ok(Some(env))
        })
        .await
        .context("load_env task panicked")?
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
                source: None,
                terminating: false,
                debug_wait: false,
            },
            observed: ContainerObserved {
                status: "running".to_string(),
                published_port: Some(8080),
                ip: None,
                ports: BTreeMap::from([("8080".to_string(), Some(8080))]),
                sync: SyncStatus::Synced,
                exit_code: None,
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

        let state = EnvState {
            network: "fghj-demo-default".to_string(),
            containers: BTreeMap::from([("svc-a".to_string(), container())]),
            sidecar_container_name: "fghj-demo-default-sidecar".to_string(),
            sidecar_ip: Some("172.20.0.5".to_string()),
            ..Default::default()
        };
        db.clone().save_env(state.clone()).await.unwrap();

        let loaded = db.clone().load_env().await.unwrap();
        assert_eq!(loaded, Some(state));

        db.clone().delete_env().await.unwrap();
        assert!(db.load_env().await.unwrap().is_none());
    }

    /// Persisted so an `fghjd` restart doesn't silently drop someone out of
    /// a halted debug session — the container is still sitting there waiting
    /// for an attach, so the switch the UI shows has to survive too.
    #[tokio::test]
    async fn the_debug_wait_switch_survives_a_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());

        let mut c = container();
        c.desired.debug_wait = true;
        let state = EnvState {
            network: "fghj-demo-default".to_string(),
            containers: BTreeMap::from([("svc-a".to_string(), c)]),
            sidecar_container_name: "fghj-demo-default-sidecar".to_string(),
            ..Default::default()
        };
        db.clone().save_env(state).await.unwrap();

        let loaded = db.load_env().await.unwrap();
        assert!(
            loaded.as_ref().unwrap().containers["svc-a"]
                .desired
                .debug_wait
        );
    }

    /// The commit a container was built from has to survive a restart, or the
    /// drift report degrades to the verdict without the evidence: `fghjd`
    /// comes back, recomputes a hash that still differs, and can say
    /// `Drifted` but no longer what it drifted *from*. It is also the one
    /// field here that cannot be re-derived afterwards — the checkout has
    /// since moved, which is the whole point.
    #[tokio::test]
    async fn the_commit_a_container_was_built_from_survives_a_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());

        let mut c = container();
        c.desired.source = Some(crate::state::ContainerSource {
            branch: Some("feature/widgets".to_string()),
            head: Some("a3f9c1d4e5b6a7c8d9e0f1a2b3c4d5e6f7a8b9c0".to_string()),
            dirty: false,
        });
        db.clone()
            .save_env(EnvState {
                containers: BTreeMap::from([("svc-a".to_string(), c)]),
                ..Default::default()
            })
            .await
            .unwrap();

        let loaded = db.load_env().await.unwrap();
        let source = loaded.as_ref().unwrap().containers["svc-a"]
            .desired
            .source
            .clone()
            .expect("source should round-trip rather than coming back as None");
        assert_eq!(source.branch.as_deref(), Some("feature/widgets"));
        assert_eq!(
            source.head.as_deref(),
            Some("a3f9c1d4e5b6a7c8d9e0f1a2b3c4d5e6f7a8b9c0")
        );
        assert!(!source.dirty);
    }

    /// A container started before `source_json` existed has to read back as
    /// `None` rather than as a default `ContainerSource`. The two say very
    /// different things: `None` is "this fghjd never recorded a commit", a
    /// zeroed struct would claim the container was built from an unknown
    /// branch at no commit with a clean tree — a finding fghj never made.
    #[tokio::test]
    async fn a_row_predating_the_source_column_reloads_as_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());

        db.clone()
            .save_env(EnvState {
                containers: BTreeMap::from([("svc-a".to_string(), container())]),
                ..Default::default()
            })
            .await
            .unwrap();
        // Exactly what an older `fghjd` left behind: the column is there
        // (the migration added it) and nothing ever wrote to it.
        db.conn
            .lock()
            .unwrap()
            .execute("UPDATE containers SET source_json = NULL", [])
            .unwrap();

        let loaded = db.load_env().await.unwrap();
        assert_eq!(
            loaded.as_ref().unwrap().containers["svc-a"].desired.source,
            None
        );
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
            .save_env(EnvState {
                containers: BTreeMap::from([("svc-a".to_string(), crashed)]),
                ..Default::default()
            })
            .await
            .unwrap();

        let restored = &db.load_env().await.unwrap().unwrap().containers["svc-a"];
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
            .save_env(EnvState {
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

        let restored = &db.load_env().await.unwrap().unwrap().containers["svc-a"];
        assert!(restored.desired.running);
        assert_eq!(restored.observed.status, "running");
    }
}
