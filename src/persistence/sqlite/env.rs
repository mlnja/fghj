#[cfg(test)]
use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::persistence::workspace_owner::WorkspaceOwner;
use crate::state::{ContainerInfo, ContainerObserved, EnvState, SyncStatus};

use super::WorkspaceDb;

/// A JSON column that no longer parses reads as empty: the next start rebuilds
/// it, and failing the load would lose every other container too.
fn json_column<T: serde::de::DeserializeOwned + Default>(raw: String) -> T {
    serde_json::from_str(&raw).unwrap_or_default()
}

impl WorkspaceDb {
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
                    "INSERT INTO containers (node_id, desired, observed) VALUES (?1, ?2, ?3)",
                    rusqlite::params![
                        c.node_id,
                        serde_json::to_string(&c.desired)?,
                        serde_json::to_string(&c.observed)?,
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

            let mut stmt = conn.prepare("SELECT node_id, desired, observed FROM containers")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (node_id, desired, observed) = row?;
                let mut observed: ContainerObserved = json_column(observed);
                // Orphanhood is re-derived from the current graph.
                if observed.sync == SyncStatus::Orphaned {
                    observed.sync = SyncStatus::Unknown;
                }
                env.containers.insert(
                    node_id.clone(),
                    ContainerInfo {
                        node_id,
                        desired: json_column(desired),
                        observed,
                        pending_action: None,
                    },
                );
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
    use crate::state::{ContainerDesired, PortRoute};

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
                ports: BTreeMap::from([("8080".to_string(), Some(8080))]),
                sync: SyncStatus::Synced,
                exit_code: None,
                interrupted: false,
                unseen: false,
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

    #[tokio::test]
    async fn an_orphaned_container_reloads_as_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());

        let mut orphan = container();
        orphan.observed.sync = SyncStatus::Orphaned;
        db.clone()
            .save_env(EnvState {
                containers: BTreeMap::from([("svc-a".to_string(), orphan)]),
                ..Default::default()
            })
            .await
            .unwrap();

        let restored = &db.load_env().await.unwrap().unwrap().containers["svc-a"];
        assert_eq!(restored.observed.sync, SyncStatus::Unknown);
    }
}
