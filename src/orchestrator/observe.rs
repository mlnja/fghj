use std::collections::BTreeMap;

use super::spec::spec_hash;
use crate::docker;
use crate::resolver::Graph;
use crate::resolver::universe::workspace_name;
use crate::state::{ContainerObserved, EnvState, SyncStatus};

use super::registry::Orchestrator;

impl Orchestrator {
    /// Re-inspects every container and reports what Docker says. Observes only;
    /// the reducer writes it down.
    pub async fn inspect_containers(&self, env: &EnvState) -> BTreeMap<String, ContainerObserved> {
        let mut updated = BTreeMap::new();
        for (node_id, c) in &env.containers {
            let name = &c.desired.container_name;
            let inspected = match c.desired.status_port.as_deref() {
                Some(p) => docker::inspect_status(&self.docker, name, p).await,
                None => docker::inspect_status(&self.docker, name, "").await,
            };
            let (status, published_port, exit_code) = match inspected {
                Ok(Some(s)) => (s.status, s.published_port, s.exit_code),
                _ => ("removed".to_string(), None, None),
            };

            let mut ports = c.observed.ports.clone();
            for (port, host_port) in ports.iter_mut() {
                *host_port = if c.desired.status_port.as_deref() == Some(port.as_str()) {
                    published_port
                } else {
                    docker::inspect_status(&self.docker, name, port)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|s| s.published_port)
                };
            }

            updated.insert(
                node_id.clone(),
                ContainerObserved {
                    status,
                    published_port,
                    ports,
                    exit_code,
                    ..c.observed.clone()
                },
            );
        }
        updated
    }

    /// Empty on a Docker error: observation is never worth failing over.
    pub async fn volume_names(&self) -> Vec<String> {
        docker::list_volumes(&self.docker, &workspace_name(&self.workspace))
            .await
            .unwrap_or_default()
    }

    /// Compares each live container's config hash with one recomputed from the
    /// current graph. Informational only: never acts on drift.
    pub async fn config_drift(&self, graph: &Graph, env: &EnvState) -> Vec<DriftReport> {
        let mut reports = Vec::new();
        for c in env.containers.values() {
            let sync = match graph.nodes.iter().find(|n| n.id == c.node_id) {
                Some(node) => match self.resolve_node_spec(graph, node, false).await {
                    Ok(spec) => {
                        if spec_hash(node, &spec) == c.desired.config_hash {
                            SyncStatus::Synced
                        } else {
                            SyncStatus::Drifted
                        }
                    }
                    Err(_) => SyncStatus::Unknown,
                },
                None => SyncStatus::Orphaned,
            };
            reports.push(DriftReport {
                node_id: c.node_id.clone(),
                sync,
            });
        }
        reports
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftReport {
    pub node_id: String,
    pub sync: SyncStatus,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::WorkspaceDb;
    use crate::state::{ContainerDesired, ContainerInfo, PortRoute};
    use std::sync::Arc;

    struct DriftingPortContainer {
        name: String,
    }

    impl DriftingPortContainer {
        fn start() -> Self {
            let name = format!(
                "fghj-refresh-test-{}",
                std::process::id().wrapping_add(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.subsec_nanos())
                        .unwrap_or(0)
                )
            );
            let status = std::process::Command::new("docker")
                .args([
                    "run",
                    "-d",
                    "--rm",
                    "--name",
                    &name,
                    "-p",
                    "127.0.0.1::8080",
                    "busybox",
                    "sleep",
                    "60",
                ])
                .status()
                .expect("failed to run `docker run` for refresh test fixture");
            assert!(
                status.success(),
                "docker run failed for refresh test fixture"
            );
            Self { name }
        }
    }

    impl Drop for DriftingPortContainer {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &self.name])
                .status();
        }
    }

    /// A container Docker republished on a new host port must report the new port.
    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn inspect_reports_the_real_port_after_docker_moves_the_published_one() {
        let container = DriftingPortContainer::start();
        let docker = Arc::new(crate::daemon::connect_docker().expect("docker client"));
        let real_port = docker::inspect_status(&docker, &container.name, "8080")
            .await
            .expect("inspect_status failed")
            .and_then(|s| s.published_port)
            .expect("container must have a real published port");

        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        let registry = Orchestrator::new(tmp.path().to_path_buf(), db, docker);

        let stale_port = real_port.wrapping_add(1).max(1);
        let stale_container = ContainerInfo {
            node_id: "svc".to_string(),
            desired: ContainerDesired {
                running: true,
                container_name: container.name.clone(),
                domain: "svc.demo.fghj.internal".to_string(),
                raw_domain: "svc.demo.fghj.raw.internal".to_string(),
                routes: vec![PortRoute {
                    domain: "svc.demo.fghj.internal".to_string(),
                    host_port: stale_port,
                    wildcard: false,
                    https: true,
                    container_port: "8080".to_string(),
                }],
                additional_hosts: Vec::new(),
                status_port: Some("8080".to_string()),
                config_hash: String::new(),
                source: None,
                terminating: false,
                debug_wait: false,
            },
            observed: ContainerObserved {
                status: "running".to_string(),
                published_port: Some(stale_port),
                ports: BTreeMap::from([("8080".to_string(), Some(stale_port))]),
                ..Default::default()
            },
            pending_action: None,
        };
        let env = EnvState {
            network: "bridge".to_string(),
            containers: BTreeMap::from([("svc".to_string(), stale_container.clone())]),
            ..Default::default()
        };

        let observed = registry.inspect_containers(&env).await;
        let o = &observed["svc"];
        assert_eq!(o.published_port, Some(real_port));
        assert_eq!(o.ports.get("8080").copied().flatten(), Some(real_port));
        assert!(stale_container.desired.running);
        assert_eq!(stale_container.desired.routes[0].host_port, stale_port);
    }
}
