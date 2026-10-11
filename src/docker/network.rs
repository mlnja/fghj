use std::collections::HashMap;

use anyhow::{Context, Result};
use bollard::Docker;
use bollard::models::NetworkCreateRequest;

/// Compose's network labels, so Docker Desktop and OrbStack can delete the
/// whole group including the network.
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
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 409, ..
        }) => Ok(()),
        Err(e) => Err(e).context("docker create_network failed"),
    }
}

pub async fn remove_network(docker: &Docker, name: &str) {
    let _ = docker.remove_network(name).await;
}
