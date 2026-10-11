//! Named volumes, labelled so an environment's can be found again.

use std::collections::HashMap;

use anyhow::{Context, Result};
use bollard::Docker;
use bollard::models::VolumeCreateRequest;
use bollard::query_parameters::ListVolumesOptionsBuilder;

/// A named volume is otherwise Docker-implicit — the daemon auto-creates
/// one the first time a container's `Binds` references a name that
/// doesn't exist yet, but never labels it. Calling this explicitly first
/// (mirrors `network::ensure_network`) attaches the same
/// `com.docker.compose.project` bookkeeping label containers/networks
/// already get. Unlike `create_network`, creating a volume that already exists
/// isn't an error — the API just returns the existing one — so there's no
/// "already exists" case to special-case.
pub async fn ensure_volume(docker: &Docker, name: &str, project: &str) -> Result<()> {
    let mut labels = HashMap::new();
    labels.insert(
        "com.docker.compose.project".to_string(),
        project.to_string(),
    );
    docker
        .create_volume(VolumeCreateRequest {
            name: Some(name.to_string()),
            labels: Some(labels),
            ..Default::default()
        })
        .await
        .context("docker create_volume failed")?;
    Ok(())
}

/// Lists the name of every Docker volume `ensure_volume` has labeled for
/// `project` (the workspace name). Used by `effects::docker::observe` to
/// discover what volumes actually exist, since `state::EnvState::volumes`
/// starts out empty and there is no other source of truth for volume
/// identity yet.
pub async fn list_volumes(docker: &Docker, project: &str) -> Result<Vec<String>> {
    let mut filters: HashMap<&str, Vec<String>> = HashMap::new();
    filters.insert(
        "label",
        vec![format!("com.docker.compose.project={project}")],
    );
    let listed = docker
        .list_volumes(Some(
            ListVolumesOptionsBuilder::new().filters(&filters).build(),
        ))
        .await
        .context("docker list_volumes failed")?;
    Ok(listed
        .volumes
        .unwrap_or_default()
        .into_iter()
        .map(|v| v.name)
        .collect())
}
