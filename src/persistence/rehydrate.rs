use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;

use crate::state::RunState;

use super::sqlite::WorkspaceDb;

/// The one place SQLite gets read to reconstruct a workspace's boot-time run
/// state — loads whatever was persisted from a previous `fghjd` lifetime and
/// reconciles each run against real docker state: a run whose containers are
/// all still alive is restored with freshly-inspected statuses, and a run
/// missing any container (removed out-of-band, or lost across a reboot with
/// no restart policy) is dropped rather than presented as running.
///
/// Deliberately more than a plain SQLite read: the reconciliation step
/// (inspecting each container's real status, dropping dead ones, deleting
/// runs that end up empty) is itself part of "what does a truthful initial
/// `WorkspaceState` look like right now" — splitting it into a separate,
/// naive "just read the rows" function would leave two different (and
/// divergent) notions of a workspace's boot state depending on which one a
/// caller reached for. `RunRegistry::new` is this function's sole caller.
pub async fn rehydrate(
    db: Arc<WorkspaceDb>,
    docker: Arc<bollard::Docker>,
) -> Result<BTreeMap<String, RunState>> {
    let persisted = db.clone().load_runs().await?;
    let mut reconciled = BTreeMap::new();
    for (run_id, mut state) in persisted {
        // Per-container, not per-run: one container having disappeared
        // (stopped, renamed, mid-recreate at exactly the moment `fghjd`
        // restarted) doesn't mean the rest of the run's containers did too.
        // Dropping the whole run's tracked list on a single miss silently
        // orphaned every other still-running container from `fghjd`'s
        // bookkeeping — including from the sidecar route table, since
        // that's built from exactly this list.
        let mut alive = BTreeMap::new();
        for (node_id, mut c) in state.containers {
            if let Ok(Some(status)) =
                crate::docker::inspect_status(&docker, &c.desired.container_name, "").await
            {
                // Only `observed` moves — what the persisted row said fghj
                // *wants* this container to be doing survives the restart
                // untouched, which is what lets a container that died while
                // `fghjd` was down come back up still reading as drifted.
                c.observed.status = status.status;
                alive.insert(node_id, c);
            }
        }
        state.containers = alive;
        if state.containers.is_empty() {
            let _ = db.clone().delete_run(run_id).await;
        } else {
            reconciled.insert(run_id, state);
        }
    }
    Ok(reconciled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ContainerDesired, ContainerInfo, ContainerObserved, RunState};

    /// A container that no longer exists in Docker (inspect returns a 404,
    /// which `docker::inspect_status` maps to `Ok(None)`) must be dropped
    /// from the reconciled state, and a run left with no surviving
    /// containers must be pruned from the db entirely — otherwise a boot
    /// after every one of a run's containers was removed out-of-band would
    /// keep presenting it to the UI as a run with zero containers forever.
    #[tokio::test]
    async fn drops_dead_containers_and_prunes_now_empty_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        let docker = Arc::new(
            bollard::Docker::connect_with_local_defaults().expect("docker client for test"),
        );

        let dead_state = RunState {
            run_id: "dead".to_string(),
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
        db.clone().save_run(dead_state).await.unwrap();

        let reconciled = rehydrate(db.clone(), docker).await.unwrap();

        assert!(reconciled.is_empty());
        assert!(db.load_runs().await.unwrap().is_empty());
    }
}
