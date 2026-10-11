use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;

use crate::state::EnvState;

use super::sqlite::WorkspaceDb;

/// Reads the persisted environment and reconciles it against Docker: live
/// containers get fresh statuses, gone ones are dropped, and an environment
/// left empty is deleted.
pub async fn rehydrate(
    db: Arc<WorkspaceDb>,
    docker: Arc<bollard::Docker>,
) -> Result<Option<EnvState>> {
    let Some(mut state) = db.clone().load_env().await? else {
        return Ok(None);
    };
    // Per container: one missing container doesn't mean the rest are gone.
    let mut alive = BTreeMap::new();
    for (node_id, mut c) in state.containers {
        if let Ok(Some(status)) =
            crate::docker::inspect_status(&docker, &c.desired.container_name, "").await
        {
            // Only `observed` moves, so a container that died while fghjd was down still
            // reads as drifted.
            c.observed.status = status.status;
            alive.insert(node_id, c);
        }
    }
    state.containers = alive;
    if state.containers.is_empty() {
        let _ = db.clone().delete_env().await;
        return Ok(None);
    }
    Ok(Some(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ContainerDesired, ContainerInfo, ContainerObserved, EnvState};

    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn drops_dead_containers_and_prunes_a_now_empty_environment() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        let docker = Arc::new(
            bollard::Docker::connect_with_local_defaults().expect("docker client for test"),
        );

        let dead_state = EnvState {
            network: "fghj-test-dead".to_string(),
            containers: BTreeMap::from([(
                "svc".to_string(),
                ContainerInfo {
                    node_id: "svc".to_string(),
                    desired: ContainerDesired {
                        running: true,
                        container_name: "fghj-rehydrate-test-does-not-exist".to_string(),
                        domain: "svc.dead.fghj".to_string(),
                        raw_domain: "svc.dead.fghj.raw.internal".to_string(),
                        ..Default::default()
                    },
                    observed: ContainerObserved {
                        status: "running".to_string(),
                        ..Default::default()
                    },
                    pending_action: None,
                },
            )]),
            sidecar_container_name: "fghj-test-dead-sidecar".to_string(),
            ..Default::default()
        };
        db.clone().save_env(dead_state).await.unwrap();

        let reconciled = rehydrate(db.clone(), docker).await.unwrap();

        assert!(reconciled.is_none());
        assert!(db.load_env().await.unwrap().is_none());
    }
}
