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

pub async fn remove_volume(docker: &Docker, name: &str) -> Result<()> {
    docker
        .remove_volume(name, None::<bollard::query_parameters::RemoveVolumeOptions>)
        .await
        .with_context(|| format!("docker remove_volume {name} failed"))
}

/// What a container's data is: each volume it mounts, named or anonymous, as
/// `name@created`, so a volume removed and made again reads as new data. With
/// no volume the data lives and dies with the container, so it's the container.
/// `Ok(None)` if the container doesn't exist.
pub async fn data_identity(docker: &Docker, container: &str) -> Result<Option<Vec<String>>> {
    let Some(inspected) = super::container::inspect(docker, container).await? else {
        return Ok(None);
    };
    let mut identity = Vec::new();
    for mount in inspected.mounts.unwrap_or_default() {
        let (Some("volume"), Some(name)) = (mount.typ.as_deref(), mount.name) else {
            continue;
        };
        let volume = docker
            .inspect_volume(&name)
            .await
            .context("docker inspect_volume failed")?;
        let created = volume
            .created_at
            .map(|at| at.unix_timestamp_nanos().to_string())
            .unwrap_or_default();
        identity.push(format!("{name}@{created}"));
    }
    if identity.is_empty() {
        identity.push(format!("container:{}", inspected.id.unwrap_or_default()));
    }
    identity.sort();
    Ok(Some(identity))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docker(args: &[&str]) {
        let status = std::process::Command::new("docker")
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("failed to run docker");
        assert!(status.success(), "docker {args:?}");
    }

    fn run(name: &str, volume: Option<&str>) {
        let mount = volume.map(|v| format!("{v}:/data"));
        let mut args = vec!["run", "-d", "--name", name];
        if let Some(mount) = &mount {
            args.extend(["-v", mount]);
        }
        args.extend(["busybox", "sleep", "60"]);
        docker(&args);
    }

    /// A seed is skipped on the same data and runs again on new data, so the
    /// identity has to tell the two apart through every way a store comes back.
    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn the_same_volume_reads_the_same_and_one_made_again_reads_new() {
        let client = crate::daemon::connect_docker().expect("docker client");
        let suffix = crate::docker::rand_suffix();
        let (store, volume) = (
            format!("fghj-data-test-{suffix}"),
            format!("fghj-data-test-{suffix}"),
        );
        let identity = || async { data_identity(&client, &store).await.unwrap() };

        assert_eq!(identity().await, None);
        run(&store, Some(&volume));
        let first = identity().await.unwrap();
        assert!(first[0].starts_with(&format!("{volume}@")), "{first:?}");

        docker(&["rm", "-f", &store]);
        run(&store, Some(&volume));
        assert_eq!(
            identity().await.unwrap(),
            first,
            "a recreated container keeps its data"
        );

        docker(&["rm", "-f", &store]);
        docker(&["volume", "rm", &volume]);
        run(&store, Some(&volume));
        assert_ne!(
            identity().await.unwrap(),
            first,
            "a volume made again is new data"
        );

        docker(&["rm", "-f", &store]);
        docker(&["volume", "rm", &volume]);
        run(&store, None);
        let bare = identity().await.unwrap();
        assert!(bare[0].starts_with("container:"), "{bare:?}");
        docker(&["rm", "-f", &store]);
        run(&store, None);
        assert_ne!(
            identity().await.unwrap(),
            bare,
            "data without a volume dies with the container"
        );
        docker(&["rm", "-f", &store]);
    }
}
