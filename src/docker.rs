use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

fn run_ok(mut cmd: Command, action: &str) -> Result<String> {
    let output = cmd.output().with_context(|| format!("failed to run {action}"))?;
    if !output.status.success() {
        bail!(
            "{action} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn ensure_network(name: &str) -> Result<()> {
    let inspect = Command::new("docker")
        .args(["network", "inspect", name])
        .output()
        .context("failed to run docker network inspect")?;
    if inspect.status.success() {
        return Ok(());
    }
    run_ok(
        {
            let mut c = Command::new("docker");
            c.args(["network", "create", name]);
            c
        },
        "docker network create",
    )?;
    Ok(())
}

pub fn remove_network(name: &str) -> Result<()> {
    let _ = Command::new("docker").args(["network", "rm", name]).output();
    Ok(())
}

/// `docker build` needs a real working tree, not a bare mirror — clone the
/// branch out of the local mirror into `dest` so it can be used as a build context.
pub fn materialize_checkout(mirror_path: &Path, branch: &str, dest: &Path) -> Result<PathBuf> {
    if dest.exists() {
        std::fs::remove_dir_all(dest).context("failed to clear stale checkout dir")?;
    }
    run_ok(
        {
            let mut c = Command::new("git");
            c.args(["clone", "--quiet", "--branch", branch, "--single-branch"])
                .arg(mirror_path)
                .arg(dest);
            c
        },
        &format!("git clone --branch {branch}"),
    )?;
    Ok(dest.to_path_buf())
}

pub fn build_image(context_dir: &Path, dockerfile: &str, tag: &str) -> Result<()> {
    run_ok(
        {
            let mut c = Command::new("docker");
            c.arg("build")
                .arg("-f")
                .arg(context_dir.join(dockerfile))
                .arg("-t")
                .arg(tag)
                .arg(context_dir);
            c
        },
        &format!("docker build -t {tag}"),
    )?;
    Ok(())
}

pub struct RunOpts<'a> {
    pub name: &'a str,
    pub network: &'a str,
    pub aliases: &'a [String],
    pub env: &'a [String],
    /// container-side ports to publish to an ephemeral localhost port
    pub ports: &'a [String],
    pub image: &'a str,
    /// stack/project id — mirrors Docker Compose's `com.docker.compose.project`
    /// label so Docker Desktop (and `docker ps`/`compose ls` tooling) groups
    /// every container in a run together, even though we never call `docker compose`.
    pub project: &'a str,
    pub service_name: &'a str,
}

pub fn run_container(opts: &RunOpts) -> Result<String> {
    let mut c = Command::new("docker");
    c.args(["run", "-d", "--name", opts.name, "--network", opts.network]);
    c.args(["--label", &format!("com.docker.compose.project={}", opts.project)]);
    c.args(["--label", &format!("com.docker.compose.service={}", opts.service_name)]);
    c.args(["--label", "com.docker.compose.oneoff=False"]);
    for alias in opts.aliases {
        c.args(["--network-alias", alias]);
    }
    for kv in opts.env {
        c.args(["-e", kv]);
    }
    for port in opts.ports {
        let container_port = port.split('/').next().unwrap_or(port);
        c.args(["-p", &format!("127.0.0.1::{container_port}")]);
    }
    c.arg(opts.image);
    run_ok(c, &format!("docker run {}", opts.name))
}

pub fn stop_and_remove(name: &str) -> Result<()> {
    let _ = Command::new("docker").args(["rm", "-f", name]).output();
    Ok(())
}

#[derive(Debug, Deserialize)]
struct InspectEntry {
    #[serde(rename = "State")]
    state: InspectState,
    #[serde(rename = "NetworkSettings")]
    network_settings: InspectNetworkSettings,
}

#[derive(Debug, Deserialize)]
struct InspectState {
    #[serde(rename = "Status")]
    status: String,
}

#[derive(Debug, Deserialize)]
struct InspectNetworkSettings {
    #[serde(rename = "Ports")]
    ports: Option<std::collections::HashMap<String, Option<Vec<InspectPortBinding>>>>,
}

#[derive(Debug, Deserialize, Clone)]
struct InspectPortBinding {
    #[serde(rename = "HostPort")]
    host_port: String,
}

pub struct ContainerStatus {
    pub status: String,
    pub published_port: Option<u16>,
}

/// Inspects a container, returning its status and the host port bound to
/// `container_port` (e.g. "8080" or "8080/tcp"), if published.
pub fn inspect_status(name: &str, container_port: &str) -> Result<Option<ContainerStatus>> {
    let output = Command::new("docker")
        .args(["inspect", name])
        .output()
        .context("failed to run docker inspect")?;
    if !output.status.success() {
        return Ok(None);
    }
    let entries: Vec<InspectEntry> = serde_json::from_slice(&output.stdout)
        .context("failed to parse docker inspect output")?;
    let Some(entry) = entries.into_iter().next() else {
        return Ok(None);
    };

    let port_key = if container_port.contains('/') {
        container_port.to_string()
    } else {
        format!("{container_port}/tcp")
    };
    let published_port = entry
        .network_settings
        .ports
        .and_then(|p| p.get(&port_key).cloned().flatten())
        .and_then(|bindings| bindings.into_iter().next())
        .and_then(|b| b.host_port.parse::<u16>().ok());

    Ok(Some(ContainerStatus {
        status: entry.state.status,
        published_port,
    }))
}

pub fn logs(name: &str, tail: usize) -> Result<String> {
    let output = Command::new("docker")
        .args(["logs", "--tail", &tail.to_string(), name])
        .output()
        .context("failed to run docker logs")?;
    let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(combined)
}
