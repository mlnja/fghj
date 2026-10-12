use std::sync::Arc;

use anyhow::Result;

use crate::state::EnvState;

use super::sqlite::WorkspaceDb;

/// Reads the persisted environment and refreshes each container's status from
/// Docker. A container Docker no longer has reads as `removed`, for the user to
/// recreate or forget; an unreachable Docker leaves the status as it was.
pub async fn rehydrate(
    db: Arc<WorkspaceDb>,
    docker: Arc<bollard::Docker>,
) -> Result<Option<EnvState>> {
    let Some(mut state) = db.clone().load_env().await? else {
        return Ok(None);
    };
    // Only `observed` moves, so a container that died while fghjd was down still
    // reads as crashed.
    for c in state.containers.values_mut() {
        match crate::docker::inspect_status(&docker, &c.desired.container_name).await {
            // A deleted `run: once` task must not keep reading as a clean exit.
            Ok(Some(status)) => {
                (c.observed.status, c.observed.exit_code) = (status.status, status.exit_code)
            }
            Ok(None) => (c.observed.status, c.observed.exit_code) = ("removed".to_string(), None),
            Err(_) => {}
        }
    }
    Ok(Some(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::state::{ContainerDesired, ContainerInfo, ContainerObserved, EnvState};

    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_container_docker_no_longer_has_reads_as_removed() {
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
                        status: "exited".to_string(),
                        exit_code: Some(0),
                        ..Default::default()
                    },
                    pending_action: None,
                },
            )]),
            sidecar_container_name: "fghj-test-dead-sidecar".to_string(),
            ..Default::default()
        };
        db.clone().save_env(dead_state).await.unwrap();

        let reconciled = rehydrate(db, docker).await.unwrap().unwrap();
        let observed = &reconciled.containers["svc"].observed;
        assert_eq!(observed.status, "removed");
        assert_eq!(observed.exit_code, None);
    }

    #[tokio::test]
    async fn an_unreachable_docker_keeps_every_container() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        let state = EnvState {
            containers: BTreeMap::from([(
                "svc".to_string(),
                ContainerInfo {
                    node_id: "svc".to_string(),
                    desired: ContainerDesired::default(),
                    observed: ContainerObserved::default(),
                    pending_action: None,
                },
            )]),
            ..Default::default()
        };
        db.clone().save_env(state.clone()).await.unwrap();

        let docker = Arc::new(crate::docker::undialled_client());
        assert_eq!(rehydrate(db.clone(), docker).await.unwrap(), Some(state));
        assert!(db.load_env().await.unwrap().is_some());
    }
}
