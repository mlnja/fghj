use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;

use anyhow::{Context, Result, bail};
use bollard::Docker;
use bollard::exec::{CreateExecOptions, ResizeExecOptions, StartExecOptions, StartExecResults};
use bollard::models::{
    ContainerCreateBody, EndpointSettings, HealthConfig, HostConfig, NetworkingConfig, PortBinding,
    RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, InspectContainerOptionsBuilder, LogsOptionsBuilder,
    RemoveContainerOptionsBuilder,
};
use futures_util::StreamExt;
use futures_util::stream::Stream;
use tokio::io::AsyncWrite;

use crate::resolver::Healthcheck;

/// Falls back to `"no"`, the CUE default, for an unknown value.
fn restart_policy_name(restart: &str) -> RestartPolicyNameEnum {
    match restart {
        "always" => RestartPolicyNameEnum::ALWAYS,
        "on-failure" => RestartPolicyNameEnum::ON_FAILURE,
        "unless-stopped" => RestartPolicyNameEnum::UNLESS_STOPPED,
        _ => RestartPolicyNameEnum::NO,
    }
}

pub struct RunOpts<'a> {
    pub name: &'a str,
    pub network: &'a str,
    pub aliases: &'a [String],
    pub env: &'a [String],
    /// `None` publishes to an ephemeral localhost port.
    pub ports: &'a [(String, Option<u16>)],
    pub image: &'a str,
    /// Empty keeps the image's `CMD`.
    pub command: &'a [String],
    /// Compose's project label, so Docker Desktop groups the containers.
    pub project: &'a str,
    pub service_name: &'a str,
    pub binds: &'a [String],
    pub restart_policy: &'a str,
    pub stop_signal: Option<&'a str>,
    /// Stamped on the container so every stop honours it, including a manual
    /// `docker stop`.
    pub stop_grace_period: u64,
    pub user: Option<&'a str>,
    pub working_dir: Option<&'a str>,
    /// fghj's own labels win on conflict.
    pub labels: &'a BTreeMap<String, String>,
    pub cap_add: &'a [String],
    pub cap_drop: &'a [String],
    pub privileged: bool,
    pub extra_hosts: &'a [String],
    pub dns: &'a [String],
    pub healthcheck: Option<&'a Healthcheck>,
    /// Only helps when that platform's image is already local: fghj doesn't pull.
    pub platform: Option<&'a str>,
}

/// Creates and starts `opts.name`, replacing any container already holding the
/// name.
pub async fn run_container(docker: &Docker, opts: &RunOpts<'_>) -> Result<()> {
    stop_and_remove(docker, opts.name).await;

    let mut labels: HashMap<String, String> = opts
        .labels
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    labels.insert(
        "com.docker.compose.project".to_string(),
        opts.project.to_string(),
    );
    labels.insert(
        "com.docker.compose.service".to_string(),
        opts.service_name.to_string(),
    );
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
        EndpointSettings {
            aliases: Some(opts.aliases.to_vec()),
            ..Default::default()
        },
    );

    let body = ContainerCreateBody {
        image: Some(opts.image.to_string()),
        cmd: if opts.command.is_empty() {
            None
        } else {
            Some(opts.command.to_vec())
        },
        env: Some(opts.env.to_vec()),
        labels: Some(labels),
        exposed_ports: Some(exposed_ports),
        user: opts.user.map(|s| s.to_string()),
        working_dir: opts.working_dir.map(|s| s.to_string()),
        stop_signal: opts.stop_signal.map(|s| s.to_string()),
        stop_timeout: Some(opts.stop_grace_period as i64),
        healthcheck: opts.healthcheck.map(|hc| HealthConfig {
            test: Some(hc.test.clone()),
            interval: hc.interval.map(|s| (s * 1_000_000_000) as i64),
            timeout: hc.timeout.map(|s| (s * 1_000_000_000) as i64),
            start_period: hc.start_period.map(|s| (s * 1_000_000_000) as i64),
            retries: hc.retries.map(|r| r as i64),
            ..Default::default()
        }),
        host_config: Some(HostConfig {
            port_bindings: Some(port_bindings),
            binds: if opts.binds.is_empty() {
                None
            } else {
                Some(opts.binds.to_vec())
            },
            restart_policy: Some(RestartPolicy {
                name: Some(restart_policy_name(opts.restart_policy)),
                maximum_retry_count: None,
            }),
            cap_add: if opts.cap_add.is_empty() {
                None
            } else {
                Some(opts.cap_add.to_vec())
            },
            cap_drop: if opts.cap_drop.is_empty() {
                None
            } else {
                Some(opts.cap_drop.to_vec())
            },
            privileged: Some(opts.privileged),
            extra_hosts: if opts.extra_hosts.is_empty() {
                None
            } else {
                Some(opts.extra_hosts.to_vec())
            },
            dns: if opts.dns.is_empty() {
                None
            } else {
                Some(opts.dns.to_vec())
            },
            ..Default::default()
        }),
        networking_config: Some(NetworkingConfig {
            endpoints_config: Some(endpoints_config),
        }),
        ..Default::default()
    };

    let mut create_opts_builder = CreateContainerOptionsBuilder::default().name(opts.name);
    if let Some(platform) = opts.platform {
        create_opts_builder = create_opts_builder.platform(platform);
    }
    let create_opts = create_opts_builder.build();
    docker
        .create_container(Some(create_opts), body)
        .await
        .with_context(|| format!("docker run {} failed", opts.name))?;

    if let Err(e) = docker.start_container(opts.name, None).await {
        // Only after a failed start: after a failed create the name isn't ours.
        let _ = docker
            .remove_container(
                opts.name,
                Some(RemoveContainerOptionsBuilder::default().force(true).build()),
            )
            .await;
        return Err(e).with_context(|| format!("docker run {} failed", opts.name));
    }
    Ok(())
}

pub async fn stop_container(docker: &Docker, name: &str) {
    let _ = docker.stop_container(name, None).await;
}

/// Stop, then remove — not `force`, which is SIGKILL and would kill a database
/// mid-write. Docker escalates after the grace period, so this can't hang.
pub async fn stop_and_remove(docker: &Docker, name: &str) {
    let _ = docker.stop_container(name, None).await;
    let _ = docker
        .remove_container(
            name,
            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
        )
        .await;
}

pub struct ContainerStatus {
    pub status: String,
    pub published_port: Option<u16>,
    /// `None` while running.
    pub exit_code: Option<i64>,
}

/// Status and the host port bound to `container_port`. `Ok(None)` if the
/// container doesn't exist.
pub async fn inspect_status(
    docker: &Docker,
    name: &str,
    container_port: &str,
) -> Result<Option<ContainerStatus>> {
    let inspected = match docker
        .inspect_container(
            name,
            Some(InspectContainerOptionsBuilder::default().build()),
        )
        .await
    {
        Ok(entry) => entry,
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => return Ok(None),
        Err(e) => return Err(e).context("docker inspect_container failed"),
    };

    let (status, exit_code) = match &inspected.state {
        Some(state) => (
            state
                .status
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            state.exit_code,
        ),
        None => ("unknown".to_string(), None),
    };

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

    Ok(Some(ContainerStatus {
        status,
        published_port,
        exit_code,
    }))
}

/// `Ok(None)` if the container doesn't exist or isn't on `network`.
pub async fn inspect_network_ip(
    docker: &Docker,
    name: &str,
    network: &str,
) -> Result<Option<String>> {
    let inspected = match docker
        .inspect_container(
            name,
            Some(InspectContainerOptionsBuilder::default().build()),
        )
        .await
    {
        Ok(entry) => entry,
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => return Ok(None),
        Err(e) => return Err(e).context("docker inspect_container failed"),
    };

    Ok(inspected
        .network_settings
        .and_then(|n| n.networks)
        .and_then(|mut networks| networks.remove(network))
        .and_then(|endpoint| endpoint.ip_address)
        .filter(|ip| !ip.is_empty()))
}

/// `Ok(None)` both when the container doesn't exist and when it has no
/// healthcheck.
pub async fn inspect_health(docker: &Docker, name: &str) -> Result<Option<String>> {
    let inspected = match docker
        .inspect_container(
            name,
            Some(InspectContainerOptionsBuilder::default().build()),
        )
        .await
    {
        Ok(entry) => entry,
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => return Ok(None),
        Err(e) => return Err(e).context("docker inspect_container failed"),
    };

    Ok(inspected
        .state
        .and_then(|s| s.health)
        .and_then(|h| h.status)
        .map(|s| s.to_string()))
}

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

pub fn logs_follow(
    docker: &Docker,
    name: &str,
    timestamps: bool,
) -> impl Stream<Item = Result<bollard::container::LogOutput, bollard::errors::Error>> + use<> {
    let options = LogsOptionsBuilder::default()
        .follow(true)
        .stdout(true)
        .stderr(true)
        .tail("0")
        .timestamps(timestamps)
        .build();
    docker.logs(name, Some(options))
}

pub struct ExecSession {
    pub id: String,
    pub output: Pin<
        Box<
            dyn Stream<Item = Result<bollard::container::LogOutput, bollard::errors::Error>> + Send,
        >,
    >,
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
}

/// `tty` merges stdout and stderr.
pub async fn exec_start(
    docker: &Docker,
    container_name: &str,
    cmd: &[String],
    user: Option<&str>,
    working_dir: Option<&str>,
    tty: bool,
) -> Result<ExecSession> {
    let create_opts = CreateExecOptions {
        attach_stdin: Some(true),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        tty: Some(tty),
        cmd: Some(cmd.to_vec()),
        user: user.map(|s| s.to_string()),
        working_dir: working_dir.map(|s| s.to_string()),
        ..Default::default()
    };
    let created = docker
        .create_exec(container_name, create_opts)
        .await
        .context("docker create_exec failed")?;

    let started = docker
        .start_exec(
            &created.id,
            Some(StartExecOptions {
                detach: false,
                tty,
                ..Default::default()
            }),
        )
        .await
        .context("docker start_exec failed")?;

    match started {
        StartExecResults::Attached { output, input } => Ok(ExecSession {
            id: created.id,
            output,
            input,
        }),
        StartExecResults::Detached => bail!("docker start_exec unexpectedly detached"),
    }
}

pub async fn exec_resize(docker: &Docker, exec_id: &str, cols: u16, rows: u16) -> Result<()> {
    docker
        .resize_exec(
            exec_id,
            ResizeExecOptions {
                width: cols,
                height: rows,
            },
        )
        .await
        .context("docker resize_exec failed")
}

/// `-1` if Docker reports no code.
pub async fn exec_exit_code(docker: &Docker, exec_id: &str) -> Result<i64> {
    let inspected = docker
        .inspect_exec(exec_id)
        .await
        .context("docker inspect_exec failed")?;
    Ok(inspected.exit_code.unwrap_or(-1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tokio::io::AsyncWriteExt;

    struct TestContainer {
        name: String,
    }

    impl TestContainer {
        fn start() -> Self {
            let name = format!(
                "fghj-exec-test-{}",
                std::process::id().wrapping_add(crate::docker::rand_suffix())
            );
            let status = Command::new("docker")
                .args([
                    "run", "-d", "--rm", "--name", &name, "busybox", "sleep", "60",
                ])
                .status()
                .expect("failed to run `docker run` for exec test fixture");
            assert!(status.success(), "docker run failed for exec test fixture");
            Self { name }
        }
    }

    impl Drop for TestContainer {
        fn drop(&mut self) {
            let _ = Command::new("docker")
                .args(["rm", "-f", &self.name])
                .status();
        }
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn exec_start_streams_output_and_reports_exit_code() {
        let docker = crate::daemon::connect_docker().expect("docker client");
        let container = TestContainer::start();

        let mut session = exec_start(
            &docker,
            &container.name,
            &["echo".to_string(), "hello from exec".to_string()],
            None,
            None,
            false,
        )
        .await
        .expect("exec_start failed");

        let mut collected = Vec::new();
        while let Some(item) = session.output.next().await {
            collected.extend_from_slice(&item.expect("exec output stream error").into_bytes());
        }
        let text = String::from_utf8_lossy(&collected);
        assert!(
            text.contains("hello from exec"),
            "unexpected exec output: {text}"
        );

        let code = exec_exit_code(&docker, &session.id)
            .await
            .expect("exec_exit_code failed");
        assert_eq!(code, 0);
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn exec_start_reports_nonzero_exit_code_and_accepts_stdin() {
        let docker = crate::daemon::connect_docker().expect("docker client");
        let container = TestContainer::start();

        let mut session = exec_start(
            &docker,
            &container.name,
            &["cat".to_string()],
            None,
            None,
            false,
        )
        .await
        .expect("exec_start failed");

        session
            .input
            .write_all(b"round trip\n")
            .await
            .expect("failed to write exec stdin");
        session
            .input
            .shutdown()
            .await
            .expect("failed to close exec stdin");

        let mut collected = Vec::new();
        while let Some(item) = session.output.next().await {
            collected.extend_from_slice(&item.expect("exec output stream error").into_bytes());
        }
        assert_eq!(String::from_utf8_lossy(&collected), "round trip\n");
        assert_eq!(
            exec_exit_code(&docker, &session.id).await.unwrap(),
            0,
            "cat should exit 0 once stdin closes"
        );

        let mut failing = exec_start(
            &docker,
            &container.name,
            &["sh".to_string(), "-c".to_string(), "exit 7".to_string()],
            None,
            None,
            false,
        )
        .await
        .expect("exec_start failed");
        while (failing.output.next().await).is_some() {}
        assert_eq!(exec_exit_code(&docker, &failing.id).await.unwrap(), 7);
    }
}
