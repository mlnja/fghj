//! Per-environment Docker networks.

use std::collections::HashMap;

use anyhow::{Context, Result};
use bollard::Docker;
use bollard::models::NetworkCreateRequest;

/// `project` mirrors Docker Compose's own network labels
/// (`com.docker.compose.network`/`.project`) — without them, Docker
/// Desktop/OrbStack can group this network's containers under `project` (by
/// their own container-level labels, set in `container::run_container`) but can't
/// tell this network belongs to that same project, since name alone isn't a
/// recognized grouping signal. That breaks "delete group" in those UIs: it
/// can find and remove the containers, but can't identify the network as
/// part of the same atomic delete, so the whole group action fails instead
/// of leaving an orphaned network behind.
pub async fn ensure_network(docker: &Docker, name: &str, project: &str) -> Result<()> {
    let mut labels = HashMap::new();
    labels.insert(
        "com.docker.compose.network".to_string(),
        "default".to_string(),
    );
    labels.insert(
        "com.docker.compose.project".to_string(),
        project.to_string(),
    );
    let result = docker
        .create_network(NetworkCreateRequest {
            name: name.to_string(),
            labels: Some(labels),
            ..Default::default()
        })
        .await;
    match result {
        Ok(_) => Ok(()),
        // a network by this name already existing is fine — that's the point
        // of "ensure"; any other failure is real.
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 409, ..
        }) => Ok(()),
        Err(e) => Err(e).context("docker create_network failed"),
    }
}

pub async fn remove_network(docker: &Docker, name: &str) {
    let _ = docker.remove_network(name).await;
}
