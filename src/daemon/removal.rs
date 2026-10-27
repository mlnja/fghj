//! Taking a workspace off fghj. It leaves the registry at once; its Docker
//! resources go in a background job. `removing.json` lists what is between
//! the two, so fghjd started after a crash finishes the job instead of
//! loading the workspace again.
//!
//! The job reads what to remove from Docker (the compose project label every
//! container of the workspace carries), not from what fghjd remembers
//! running, so it can be run again from any point and catches containers of
//! runs fghjd has forgotten. Removing is discarding: the workspace's volumes,
//! and the data in them, go too.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use futures_util::future::join_all;

use crate::activity::{Activity, ActivityHandle, ActivityKind, ActivityLog};
use crate::orchestrator::naming::{network_name, sidecar_name};
use crate::orchestrator::route_table::sidecar_routes_dir;
use crate::resolver::universe::workspace_name;
use crate::{docker, persistence};

pub(crate) const ACTIVITY_KEY: &str = "remove";

/// Removals that haven't finished, by workspace id, next to the index and in
/// its format.
pub(crate) struct Removals {
    path: PathBuf,
    lock: Mutex<()>,
}

impl Removals {
    pub(crate) fn beside(index_path: &Path) -> Self {
        Self {
            path: index_path.with_file_name("removing.json"),
            lock: Mutex::new(()),
        }
    }

    pub(crate) fn pending(&self) -> HashMap<String, PathBuf> {
        let _guard = self.lock.lock().unwrap();
        persistence::load_index(&self.path)
    }

    pub(crate) fn add(&self, id: &str, path: &Path) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let mut pending = persistence::load_index(&self.path);
        pending.insert(id.to_string(), path.to_path_buf());
        persistence::save_index(&self.path, &pending)
    }

    fn finish(&self, id: &str) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let mut pending = persistence::load_index(&self.path);
        pending.remove(id);
        persistence::save_index(&self.path, &pending)
    }
}

/// Joins the job if it is already running.
pub(crate) fn spawn_teardown(
    docker: Arc<bollard::Docker>,
    activities: &Arc<ActivityLog>,
    removals: Arc<Removals>,
    id: &str,
    path: PathBuf,
) -> Activity {
    let job = match activities.start(Some(id), ActivityKind::Remove, ACTIVITY_KEY) {
        Ok(job) => job,
        Err(running) => return *running,
    };
    let snapshot = job.snapshot();
    let id = id.to_string();
    tokio::spawn(async move {
        let result = teardown(&docker, &path, &job)
            .await
            .and_then(|()| removals.finish(&id));
        if let Err(e) = &result {
            crate::daemon_log::warn(format!(
                "fghjd: removing workspace {id} ({}) failed, retried on the next start: {e:#}",
                path.display()
            ));
        }
        job.settle(&result);
    });
    snapshot
}

/// Three steps, each a child of `job` listed from the start: the
/// containers (and the routes their sidecar served), the network, the
/// volumes. Each needs the one before it gone.
async fn teardown(docker: &bollard::Docker, path: &Path, job: &ActivityHandle) -> Result<()> {
    let name = workspace_name(path);
    let network = network_name(&name);
    job.append(&format!("removing {} ({network})\n", path.display()));
    let [containers, net, volumes] =
        ["containers", "network", "volumes"].map(|k| job.queued_child(k));

    containers
        .step(remove_containers(docker, &name, &network, &containers))
        .await?;
    net.step(remove_network(docker, &network, &net)).await?;
    volumes
        .step(remove_volumes(docker, &name, &volumes))
        .await?;
    job.append("removed\n");
    Ok(())
}

/// Containers are listed again after each sweep: one a cancelled start had
/// already asked Docker for can appear after the first listing.
async fn remove_containers(
    docker: &bollard::Docker,
    name: &str,
    network: &str,
    step: &ActivityHandle,
) -> Result<()> {
    const SWEEPS: usize = 3;
    for _ in 0..SWEEPS {
        let mut found: Vec<String> = docker::list_statuses(docker, network)
            .await
            .context("failed to list the workspace's containers")?
            .into_keys()
            .collect();
        if found.is_empty() {
            break;
        }
        found.sort();
        for container in &found {
            step.append(&format!("removing container {container}\n"));
        }
        join_all(
            found
                .iter()
                .map(|container| docker::discard(docker, container)),
        )
        .await;
    }
    // Labelled like the rest; by name too, in case one predates the label.
    docker::discard(docker, &sidecar_name(name)).await;

    let left = docker::list_statuses(docker, network)
        .await
        .context("failed to list the workspace's containers")?;
    if !left.is_empty() {
        let mut left: Vec<String> = left.into_keys().collect();
        left.sort();
        bail!("containers still there: {}", left.join(", "));
    }

    let routes = sidecar_routes_dir(network);
    if routes.exists() {
        std::fs::remove_dir_all(&routes)
            .with_context(|| format!("failed to remove {}", routes.display()))?;
    }
    Ok(())
}

async fn remove_network(
    docker: &bollard::Docker,
    network: &str,
    step: &ActivityHandle,
) -> Result<()> {
    step.append(&format!("removing network {network}\n"));
    docker::remove_network(docker, network).await;
    if docker.inspect_network(network, None).await.is_ok() {
        bail!("network {network} is still there");
    }
    Ok(())
}

/// Labelled with the workspace's name, not its network: see `ensure_volume`.
async fn remove_volumes(docker: &bollard::Docker, name: &str, step: &ActivityHandle) -> Result<()> {
    let mut volumes = docker::list_volumes(docker, name)
        .await
        .context("failed to list the workspace's volumes")?;
    volumes.sort();
    for volume in &volumes {
        step.append(&format!("removing volume {volume}\n"));
        docker::remove_volume(docker, volume).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docker_cli(args: &[&str]) {
        let status = std::process::Command::new("docker")
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("docker CLI");
        assert!(status.success(), "docker {args:?} failed");
    }

    /// Running and exited containers go, then the network, then the volumes:
    /// the labelled one and the anonymous one an image declared. A second run
    /// over nothing succeeds.
    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn teardown_removes_what_docker_has_under_the_label_volumes_included() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join(format!("rm-test-{}", crate::docker::rand_suffix()));
        let network = network_name(&workspace_name(&path));
        let label = format!("com.docker.compose.project={network}");
        let name = workspace_name(&path);
        let volume = format!("{network}-vol");
        let docker = crate::daemon::connect_docker().expect("docker client");

        docker::ensure_network(&docker, &network, &network)
            .await
            .unwrap();
        docker::ensure_volume(&docker, &volume, &name)
            .await
            .unwrap();
        for (suffix, cmd) in [("up", "sleep 300"), ("exited", "true")] {
            let name = format!("{network}-{suffix}");
            docker_cli(&[
                "run",
                "-d",
                "--name",
                &name,
                "--label",
                &label,
                "--network",
                &network,
                "-v",
                &format!("{volume}:/data"),
                "-v",
                "/anon",
                "busybox",
                "sh",
                "-c",
                cmd,
            ]);
        }

        let anonymous: Vec<String> = docker
            .inspect_container(&format!("{network}-up"), None)
            .await
            .unwrap()
            .mounts
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| m.name)
            .filter(|n| *n != volume)
            .collect();
        assert_eq!(anonymous.len(), 1, "{anonymous:?}");

        let activities = Arc::new(ActivityLog::default());
        let job = activities
            .start(Some("test"), ActivityKind::Remove, ACTIVITY_KEY)
            .unwrap();
        teardown(&docker, &path, &job).await.unwrap();

        assert!(
            docker::list_statuses(&docker, &network)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(docker.inspect_network(&network, None).await.is_err());
        assert!(docker.inspect_volume(&volume).await.is_err());
        assert!(docker.inspect_volume(&anonymous[0]).await.is_err());
        assert!(job.snapshot().log.contains(&format!("{network}-exited")));
        assert!(
            job.snapshot()
                .log
                .contains(&format!("removing volume {volume}"))
        );
        let steps = activities.list(Some("test"));
        for key in ["containers", "network", "volumes"] {
            let step = steps.iter().find(|a| a.key == key).unwrap();
            assert_eq!(step.status, crate::activity::ActivityStatus::Done, "{key}");
        }

        teardown(&docker, &path, &job).await.unwrap();
    }

    #[test]
    fn removals_round_trip_beside_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let removals = Removals::beside(&tmp.path().join("workspaces.json"));
        removals.add("ws-1", Path::new("/w/one")).unwrap();
        removals.add("ws-2", Path::new("/w/two")).unwrap();
        removals.finish("ws-1").unwrap();
        assert!(tmp.path().join("removing.json").exists());
        let pending = removals.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending["ws-2"], PathBuf::from("/w/two"));
    }
}
