#[cfg(test)]
use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::persistence::workspace_owner::WorkspaceOwner;
use crate::state::{ContainerDesired, ContainerInfo, ContainerObserved, EnvState};

use super::WorkspaceDb;

/// A JSON column that no longer parses into the current shape reads as
/// empty: the next start rebuilds it, and failing the load would lose every
/// other container too.
fn json_column<T: serde::de::DeserializeOwned + Default>(raw: String) -> T {
    serde_json::from_str(&raw).unwrap_or_default()
}

impl WorkspaceDb {
    /// Refreshed on every `wire`: the captured `ssh_auth_sock` is only valid
    /// for the login session that sent it.
    pub async fn set_owner(self: Arc<Self>, owner: WorkspaceOwner) -> Result<()> {
        tokio::task::spawn_blocking(move || {
            self.conn.lock().unwrap().execute(
                "INSERT OR REPLACE INTO owner (id, uid, gid, home, ssh_auth_sock) VALUES (0, ?1, ?2, ?3, ?4)",
                rusqlite::params![owner.uid, owner.gid, owner.home, owner.ssh_auth_sock],
            )?;
            Ok(())
        })
        .await
        .context("set_owner task panicked")?
    }

    pub async fn load_owner(self: Arc<Self>) -> Result<Option<WorkspaceOwner>> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let result = conn.query_row(
                "SELECT uid, gid, home, ssh_auth_sock FROM owner",
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
            tx.execute(
                "INSERT OR REPLACE INTO env (id, network, sidecar_container_name, sidecar_ip) VALUES (0, ?1, ?2, ?3)",
                rusqlite::params![state.network, state.sidecar_container_name, state.sidecar_ip],
            )?;
            tx.execute("DELETE FROM containers", [])?;
            for c in state.containers.values() {
                tx.execute(
                    "INSERT INTO containers (node_id, container_name, status, published_port, domain, raw_domain, routes_json, additional_hosts_json, ports_json, status_port, config_hash, synced, desired_running, terminating, exit_code, debug_wait, source_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                    rusqlite::params![
                        c.node_id,
                        c.desired.container_name,
                        c.observed.status,
                        c.observed.published_port,
                        c.desired.domain,
                        c.desired.raw_domain,
                        serde_json::to_string(&c.desired.routes)?,
                        serde_json::to_string(&c.desired.additional_hosts)?,
                        serde_json::to_string(&c.observed.ports)?,
                        c.desired.status_port,
                        c.desired.config_hash,
                        Option::<bool>::from(c.observed.sync),
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
            self.conn
                .lock()
                .unwrap()
                .execute_batch("DELETE FROM containers; DELETE FROM env;")?;
            Ok(())
        })
        .await
        .context("delete_env task panicked")?
    }

    pub async fn load_env(self: Arc<Self>) -> Result<Option<EnvState>> {
        tokio::task::spawn_blocking(move || {
            let conn = self.conn.lock().unwrap();
            let env = conn.query_row(
                "SELECT network, sidecar_container_name, sidecar_ip FROM env",
                [],
                |row| {
                    Ok(EnvState {
                        network: row.get(0)?,
                        sidecar_container_name: row.get(1)?,
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
                "SELECT node_id, container_name, status, published_port, domain, raw_domain, routes_json, additional_hosts_json, ports_json, status_port, config_hash, synced, desired_running, terminating, exit_code, debug_wait, source_json FROM containers",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(ContainerInfo {
                    node_id: row.get(0)?,
                    desired: ContainerDesired {
                        container_name: row.get(1)?,
                        domain: row.get(4)?,
                        raw_domain: row.get(5)?,
                        routes: json_column(row.get(6)?),
                        additional_hosts: json_column(row.get(7)?),
                        status_port: row.get(9)?,
                        config_hash: row.get(10)?,
                        running: row.get(12)?,
                        terminating: row.get(13)?,
                        debug_wait: row.get(15)?,
                        source: json_column(row.get(16)?),
                    },
                    observed: ContainerObserved {
                        status: row.get(2)?,
                        published_port: row.get(3)?,
                        // An address only means something on the network the
                        // container is attached to right now.
                        ip: None,
                        ports: json_column(row.get(8)?),
                        sync: row.get::<_, Option<bool>>(11)?.into(),
                        exit_code: row.get(14)?,
                    },
                    // An action in flight can't survive the process running it.
                    pending_action: None,
                })
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
    use crate::state::{PortRoute, SyncStatus};

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
    async fn workspace_db_round_trips_owner_and_env() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());

        let owner = WorkspaceOwner {
            uid: 501,
            gid: 20,
            home: "/Users/someone".to_string(),
            ssh_auth_sock: Some("/tmp/agent.sock".to_string()),
        };
        db.clone().set_owner(owner.clone()).await.unwrap();
        db.clone().set_owner(owner.clone()).await.unwrap();
        assert_eq!(db.clone().load_owner().await.unwrap(), Some(owner));

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
}
