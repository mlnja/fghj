use std::collections::HashMap;

use anyhow::{Context, Result};
use bollard::Docker;
use bollard::models::VolumeCreateRequest;
use bollard::query_parameters::ListVolumesOptionsBuilder;

/// Labels the volume so `list_volumes` finds it; Docker would otherwise
/// create it unlabelled on first use.
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
