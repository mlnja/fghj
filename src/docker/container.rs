use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;

use anyhow::{Context, Result, bail};
use bollard::Docker;
use bollard::exec::{CreateExecOptions, ResizeExecOptions, StartExecOptions, StartExecResults};
use bollard::models::{
    ContainerCreateBody, ContainerInspectResponse, EndpointIpamConfig, EndpointSettings,
    HealthConfig, HostConfig, NetworkingConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, InspectContainerOptionsBuilder, ListContainersOptionsBuilder,
    LogsOptionsBuilder, RemoveContainerOptionsBuilder,
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
    /// A fixed address on `network`, which must have an explicit subnet.
    /// `None` takes whatever Docker hands out.
    pub ipv4_address: Option<&'a str>,
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
    /// Only picks among local images; `docker::pull_image` fetches one.
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
    // Compose (and so OrbStack's and Docker Desktop's Delete) skips a
    // container without it: the project looks empty and only its network goes.
    let hash = labels.get("fghj.config_hash").cloned().unwrap_or_default();
    labels.insert("com.docker.compose.config-hash".to_string(), hash);

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
            ipam_config: opts.ipv4_address.map(|ip| EndpointIpamConfig {
                ipv4_address: Some(ip.to_string()),
                ..Default::default()
            }),
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

/// The same container again: stop (the grace period applies), then start.
pub async fn restart_container(docker: &Docker, name: &str) -> Result<()> {
    docker
        .restart_container(name, None)
        .await
        .with_context(|| format!("docker restart {name} failed"))
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

/// `stop_and_remove`, and the anonymous volumes the container's image
/// declared (postgres keeps its data in one): for a container that won't be
/// made again.
pub async fn discard(docker: &Docker, name: &str) {
    let _ = docker.stop_container(name, None).await;
    let _ = docker
        .remove_container(
            name,
            Some(
                RemoveContainerOptionsBuilder::default()
                    .force(true)
                    .v(true)
                    .build(),
            ),
        )
        .await;
}

/// `Ok(None)` if the container doesn't exist.
pub(super) async fn inspect(
    docker: &Docker,
    name: &str,
) -> Result<Option<ContainerInspectResponse>> {
    match docker
        .inspect_container(
            name,
            Some(InspectContainerOptionsBuilder::default().build()),
        )
        .await
    {
        Ok(entry) => Ok(Some(entry)),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(None),
        Err(e) => Err(e).context("docker inspect_container failed"),
    }
}

pub struct ContainerStatus {
    pub status: String,
    /// `None` while running.
    pub exit_code: Option<i64>,
    host_ports: HashMap<String, u16>,
}

impl ContainerStatus {
    /// The host port Docker bound to `container_port`; a bare number means tcp.
    pub fn host_port(&self, container_port: &str) -> Option<u16> {
        if container_port.contains('/') {
            self.host_ports.get(container_port).copied()
        } else {
            self.host_ports
                .get(&format!("{container_port}/tcp"))
                .copied()
        }
    }
}

/// `Ok(None)` if the container doesn't exist.
pub async fn inspect_status(docker: &Docker, name: &str) -> Result<Option<ContainerStatus>> {
    let Some(inspected) = inspect(docker, name).await? else {
        return Ok(None);
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
    let host_ports = inspected
        .network_settings
        .and_then(|n| n.ports)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(key, bindings)| {
            let port = bindings?.into_iter().next()?.host_port?.parse().ok()?;
            Some((key, port))
        })
        .collect();

    Ok(Some(ContainerStatus {
        status,
        exit_code,
        host_ports,
    }))
}

/// Every container of compose `project`, stopped ones included, by name: one
/// call in place of an inspect per container. A container missing here doesn't
/// exist.
pub async fn list_statuses(
    docker: &Docker,
    project: &str,
) -> Result<HashMap<String, ContainerStatus>> {
    let mut filters: HashMap<&str, Vec<String>> = HashMap::new();
    filters.insert(
        "label",
        vec![format!("com.docker.compose.project={project}")],
    );
    let listed = docker
        .list_containers(Some(
            ListContainersOptionsBuilder::new()
                .all(true)
                .filters(&filters)
                .build(),
        ))
        .await
        .context("docker list_containers failed")?;

    let mut statuses = HashMap::new();
    for summary in listed {
        let Some(name) = summary
            .names
            .as_ref()
            .and_then(|names| names.first())
            .map(|n| n.trim_start_matches('/').to_string())
        else {
            continue;
        };
        let status = summary
            .state
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        let exit_code = match summary.status.as_deref().and_then(exit_code_in) {
            Some(code) => Some(code),
            // Rare: the text says `Dead`, or a format this doesn't know.
            None if matches!(status.as_str(), "exited" | "dead") => inspect_status(docker, &name)
                .await?
                .and_then(|s| s.exit_code),
            // What inspect reports for a container that hasn't exited.
            None => Some(0),
        };
        let host_ports = summary
            .ports
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| {
                let typ = p.typ.map(|t| t.to_string()).filter(|t| !t.is_empty());
                let key = format!("{}/{}", p.private_port, typ.as_deref().unwrap_or("tcp"));
                Some((key, p.public_port?))
            })
            .collect();
        statuses.insert(
            name,
            ContainerStatus {
                status,
                exit_code,
                host_ports,
            },
        );
    }
    Ok(statuses)
}

/// The code in Docker's status text: `Exited (137) 5 seconds ago`,
/// `Restarting (1) 2 seconds ago`.
fn exit_code_in(text: &str) -> Option<i64> {
    let (_, rest) = text.split_once('(')?;
    let (code, _) = rest.split_once(')')?;
    code.trim().parse().ok()
}

/// `Ok(None)` if the container doesn't exist or isn't on `network`.
/// Force-removes every container whose Compose service label is `service`, in
/// any project. How many went.
pub async fn remove_service_everywhere(docker: &Docker, service: &str) -> Result<usize> {
    let mut filters: HashMap<&str, Vec<String>> = HashMap::new();
    filters.insert(
        "label",
        vec![format!("com.docker.compose.service={service}")],
    );
    let listed = docker
        .list_containers(Some(
            ListContainersOptionsBuilder::new()
                .all(true)
                .filters(&filters)
                .build(),
        ))
        .await
        .context("docker list_containers failed")?;
    let mut removed = 0;
    for id in listed.into_iter().filter_map(|c| c.id) {
        docker
            .remove_container(
                &id,
                Some(RemoveContainerOptionsBuilder::default().force(true).build()),
            )
            .await
            .with_context(|| format!("failed to remove container {id}"))?;
        removed += 1;
    }
    Ok(removed)
}

pub async fn inspect_network_ip(
    docker: &Docker,
    name: &str,
    network: &str,
) -> Result<Option<String>> {
    Ok(inspect(docker, name)
        .await?
        .and_then(|i| i.network_settings)
        .and_then(|n| n.networks)
        .and_then(|mut networks| networks.remove(network))
        .and_then(|endpoint| endpoint.ip_address)
        .filter(|ip| !ip.is_empty()))
}

/// `Ok(None)` both when the container doesn't exist and when it has no
/// healthcheck.
pub async fn inspect_health(docker: &Docker, name: &str) -> Result<Option<String>> {
    Ok(inspect(docker, name)
        .await?
        .and_then(|i| i.state)
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

/// Where a followed stream starts: what's printed from now on, or everything
/// since a moment, in whole seconds.
#[derive(Debug, Clone, Copy)]
pub enum LogsFrom {
    Now,
    /// `0` is the container's whole output.
    Since(i32),
}

pub fn logs_follow(
    docker: &Docker,
    name: &str,
    timestamps: bool,
    from: LogsFrom,
) -> impl Stream<Item = Result<bollard::container::LogOutput, bollard::errors::Error>> + use<> {
    let options = LogsOptionsBuilder::default()
        .follow(true)
        .stdout(true)
        .stderr(true)
        .timestamps(timestamps);
    let options = match from {
        LogsFrom::Now => options.tail("0"),
        LogsFrom::Since(at) => options.since(at),
    }
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

    #[test]
    fn the_exit_code_comes_out_of_dockers_status_text() {
        assert_eq!(exit_code_in("Exited (137) 5 seconds ago"), Some(137));
        assert_eq!(exit_code_in("Exited (0) About a minute ago"), Some(0));
        assert_eq!(exit_code_in("Restarting (1) 2 seconds ago"), Some(1));
        assert_eq!(exit_code_in("Up 3 hours (healthy)"), None);
        assert_eq!(exit_code_in("Up 5 minutes"), None);
        assert_eq!(exit_code_in("Dead"), None);
    }
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
