use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use bollard::body_full;
use bollard::models::{
    ContainerCreateBody, EndpointSettings, HostConfig, NetworkCreateRequest, NetworkingConfig,
    PortBinding,
};
use bollard::query_parameters::{
    BuildImageOptionsBuilder, CreateContainerOptionsBuilder, InspectContainerOptionsBuilder,
    LogsOptionsBuilder, RemoveContainerOptionsBuilder,
};
use bollard::Docker;
use futures_util::stream::Stream;
use futures_util::StreamExt;

pub async fn ensure_network(docker: &Docker, name: &str) -> Result<()> {
    let result = docker
        .create_network(NetworkCreateRequest {
            name: name.to_string(),
            ..Default::default()
        })
        .await;
    match result {
        Ok(_) => Ok(()),
        // a network by this name already existing is fine — that's the point
        // of "ensure"; any other failure is real.
        Err(bollard::errors::Error::DockerResponseServerError { status_code: 409, .. }) => Ok(()),
        Err(e) => Err(e).context("docker create_network failed"),
    }
}

pub async fn remove_network(docker: &Docker, name: &str) {
    let _ = docker.remove_network(name).await;
}

/// `docker build` needs a real working tree, not a bare mirror — clone the
/// branch out of the local mirror into `dest` so it can be used as a build context.
pub async fn materialize_checkout(mirror_path: &Path, branch: &str, dest: &Path) -> Result<PathBuf> {
    let mirror_path = mirror_path.to_path_buf();
    let branch = branch.to_string();
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if dest.exists() {
            std::fs::remove_dir_all(&dest).context("failed to clear stale checkout dir")?;
        }
        let status = Command::new("git")
            .args(["clone", "--quiet", "--branch", &branch, "--single-branch"])
            .arg(&mirror_path)
            .arg(&dest)
            .status()
            .with_context(|| format!("failed to run git clone --branch {branch}"))?;
        if !status.success() {
            bail!("git clone --branch {branch} failed");
        }
        Ok(dest)
    })
    .await
    .context("materialize_checkout task panicked")?
}

pub async fn build_image(docker: &Docker, context_dir: &Path, dockerfile: &str, tag: &str) -> Result<()> {
    let context_dir = context_dir.to_path_buf();
    let tar_bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_dir_all("", &context_dir)
            .with_context(|| format!("failed to tar build context {}", context_dir.display()))?;
        builder.into_inner().context("failed to finalize build context tar")
    })
    .await
    .context("tar task panicked")??;

    let options = BuildImageOptionsBuilder::default()
        .dockerfile(dockerfile)
        .t(tag)
        .rm(true)
        .build();

    let mut stream = docker.build_image(options, None, Some(body_full(tar_bytes.into())));
    while let Some(item) = stream.next().await {
        let info = item.context("docker build_image stream error")?;
        if let Some(detail) = info.error_detail {
            bail!("docker build -t {tag} failed: {}", detail.message.unwrap_or_default());
        }
    }
    Ok(())
}

pub struct RunOpts<'a> {
    pub name: &'a str,
    pub network: &'a str,
    pub aliases: &'a [String],
    pub env: &'a [String],
    /// container-side ports to publish, each with an optional fixed
    /// host-side port — `None` publishes to a random ephemeral localhost
    /// port (the default), `Some(p)` binds exactly `127.0.0.1:p`.
    pub ports: &'a [(String, Option<u16>)],
    pub image: &'a str,
    /// stack/project id — mirrors Docker Compose's `com.docker.compose.project`
    /// label so Docker Desktop (and `docker ps`/`compose ls` tooling) groups
    /// every container in a run together, even though we never call `docker compose`.
    pub project: &'a str,
    pub service_name: &'a str,
}

pub async fn run_container(docker: &Docker, opts: &RunOpts<'_>) -> Result<()> {
    let mut labels = HashMap::new();
    labels.insert("com.docker.compose.project".to_string(), opts.project.to_string());
    labels.insert("com.docker.compose.service".to_string(), opts.service_name.to_string());
    labels.insert("com.docker.compose.oneoff".to_string(), "False".to_string());

    let mut exposed_ports = Vec::new();
    let mut port_bindings = HashMap::new();
    for (port, host_port) in opts.ports {
        let container_port = port.split('/').next().unwrap_or(port);
        let key = format!("{container_port}/tcp");
        exposed_ports.push(key.clone());
        port_bindings.insert(
            key,
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".to_string()),
                host_port: host_port.map(|p| p.to_string()),
            }]),
        );
    }

    let mut endpoints_config = HashMap::new();
    endpoints_config.insert(
        opts.network.to_string(),
        EndpointSettings { aliases: Some(opts.aliases.to_vec()), ..Default::default() },
    );

    let body = ContainerCreateBody {
        image: Some(opts.image.to_string()),
        env: Some(opts.env.to_vec()),
        labels: Some(labels),
        exposed_ports: Some(exposed_ports),
        host_config: Some(HostConfig {
            port_bindings: Some(port_bindings),
            ..Default::default()
        }),
        networking_config: Some(NetworkingConfig { endpoints_config: Some(endpoints_config) }),
        ..Default::default()
    };

    let create_opts = CreateContainerOptionsBuilder::default().name(opts.name).build();
    let result = async {
        docker.create_container(Some(create_opts), body).await?;
        docker.start_container(opts.name, None).await?;
        Ok::<(), bollard::errors::Error>(())
    }
    .await;

    if let Err(e) = result {
        // Docker can leave a container behind in `Created` state if start
        // fails post-create (e.g. network attach failure) — best-effort clean
        // it up so callers never leak a dangling container blocking retries.
        let _ = docker
            .remove_container(opts.name, Some(RemoveContainerOptionsBuilder::default().force(true).build()))
            .await;
        return Err(e).with_context(|| format!("docker run {} failed", opts.name));
    }
    Ok(())
}

pub async fn stop_and_remove(docker: &Docker, name: &str) {
    let _ = docker
        .remove_container(name, Some(RemoveContainerOptionsBuilder::default().force(true).build()))
        .await;
}

pub struct ContainerStatus {
    pub status: String,
    pub published_port: Option<u16>,
}

/// Inspects a container, returning its status and the host port bound to
/// `container_port` (e.g. "8080" or "8080/tcp"), if published. Returns
/// `Ok(None)` if the container doesn't exist, rather than erroring — this is
/// used as the "does it exist" check everywhere.
pub async fn inspect_status(docker: &Docker, name: &str, container_port: &str) -> Result<Option<ContainerStatus>> {
    let inspected = match docker.inspect_container(name, Some(InspectContainerOptionsBuilder::default().build())).await {
        Ok(entry) => entry,
        Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => return Ok(None),
        Err(e) => return Err(e).context("docker inspect_container failed"),
    };

    let status = inspected
        .state
        .and_then(|s| s.status)
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let port_key = if container_port.contains('/') {
        container_port.to_string()
    } else {
        format!("{container_port}/tcp")
    };
    let published_port = inspected
        .network_settings
        .and_then(|n| n.ports)
        .and_then(|p| p.get(&port_key).cloned().flatten())
        .and_then(|bindings| bindings.into_iter().next())
        .and_then(|b| b.host_port)
        .and_then(|p| p.parse::<u16>().ok());

    Ok(Some(ContainerStatus { status, published_port }))
}

/// One-shot: fetches the last `tail` lines and returns them as a string.
pub async fn logs_tail(docker: &Docker, name: &str, tail: usize) -> Result<String> {
    let options = LogsOptionsBuilder::default()
        .stdout(true)
        .stderr(true)
        .tail(&tail.to_string())
        .build();
    let mut stream = docker.logs(name, Some(options));
    let mut combined = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => combined.push_str(&chunk.to_string()),
            Err(e) => {
                combined.push_str(&format!("[fghj: log read error: {e}]\n"));
                break;
            }
        }
    }
    Ok(combined)
}

/// Continuous: follows the container's log output, never ending until the
/// container stops or the caller drops the stream. Used by the SSE endpoint.
///
/// Bollard's `Docker::logs` clones what it needs out of `&self` before
/// returning (see its `process_request`), so the returned stream owns
/// everything it needs and isn't tied to `docker`'s or `name`'s lifetime —
/// safe to return from a handler after `docker`/`name` go out of scope.
pub fn logs_follow(docker: &Docker, name: &str) -> impl Stream<Item = Result<bollard::container::LogOutput, bollard::errors::Error>> + use<> {
    let options = LogsOptionsBuilder::default()
        .follow(true)
        .stdout(true)
        .stderr(true)
        .tail("0")
        .build();
    docker.logs(name, Some(options))
}
