//! Read-only reconciliation: polls Docker for what is actually true and
//! folds the answer back into the recorded `RunState`.

use std::collections::BTreeMap;

use super::spec::spec_hash;
use crate::docker;
use crate::resolver::Graph;
use crate::state::{ContainerInfo, ContainerObserved, RunState};

use super::registry::RunRegistry;

impl RunRegistry {
    /// Re-inspects every live run's containers against real docker state and
    /// updates their recorded status, published port, and per-port host
    /// bindings — including flagging any container that's vanished (e.g.
    /// `docker rm`'d by hand, outside fghj) as `"removed"` — so the next
    /// `/runs` poll (and `web::proxy::serve_https`'s routing, via
    /// `state::query::resolve_route`) reflects reality instead of a snapshot
    /// frozen at whenever the run last started or was persisted. Purely
    /// observational with respect to Docker: it only re-reads state Docker
    /// already changed on its own, and never starts, stops, or recreates a
    /// container itself.
    ///
    /// Writes into `observed` only, never `desired` — that asymmetry is the
    /// entire reason the two are separate fields. A container Docker
    /// restarted onto a different ephemeral host port needs no route
    /// rewriting either: `state::query::live_host_port` re-joins each stored
    /// route against these freshly-observed `ports` at read time, so there
    /// is one correction, at the point of use, instead of a stored copy to
    /// keep in step.
    ///
    /// Snapshots each container's inspectable identity (name, the port key
    /// `status`/`published_port` are read from) while holding the lock,
    /// inspects it all without holding it (inspection is an async docker call
    /// per port), then re-locks to write results back — the lock is never
    /// held across an `.await`.
    pub async fn refresh(&self) {
        let snapshot: Vec<(String, BTreeMap<String, ContainerInfo>)> = {
            let runs = self.runs.lock().unwrap();
            runs.iter()
                .map(|(run_id, state)| (run_id.clone(), state.containers.clone()))
                .collect()
        };

        let mut results: Vec<(String, BTreeMap<String, ContainerObserved>)> = Vec::new();
        for (run_id, containers) in snapshot {
            let mut updated = BTreeMap::new();
            for (node_id, c) in containers {
                let name = &c.desired.container_name;
                let inspected = match c.desired.status_port.as_deref() {
                    Some(p) => docker::inspect_status(&self.docker, name, p).await,
                    None => docker::inspect_status(&self.docker, name, "").await,
                };
                let (status, published_port) = match inspected {
                    Ok(Some(s)) => (s.status, s.published_port),
                    _ => ("removed".to_string(), None),
                };

                // Re-inspects every declared port, not just `status_port` —
                // same "one inspect per remaining port" approach `start_node`
                // uses, reusing the inspect already done above for
                // `status_port`'s own binding rather than re-querying it.
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
                    node_id,
                    ContainerObserved {
                        status,
                        published_port,
                        ports,
                        // Neither the container's IP nor its config-drift
                        // verdict is something this inspection looks at;
                        // carrying the previous readings forward keeps this
                        // from clobbering whoever does own them.
                        ..c.observed
                    },
                );
            }
            results.push((run_id, updated));
        }

        let mut changed_states: Vec<RunState> = Vec::new();
        {
            let mut runs = self.runs.lock().unwrap();
            for (run_id, updated) in results {
                if let Some(state) = runs.get_mut(&run_id) {
                    let mut changed = false;
                    for (node_id, observed) in updated {
                        if let Some(c) = state.containers.get_mut(&node_id)
                            && c.observed != observed
                        {
                            c.observed = observed;
                            changed = true;
                        }
                    }
                    if changed {
                        changed_states.push(state.clone());
                    }
                }
            }
        }
        for state in changed_states {
            let _ = self.db.clone().save_run(state).await;
        }
    }

    /// The read-only Docker-volume counterpart to `refresh` — lists what
    /// `docker::list_run_volumes` actually finds for `run_id` right now.
    /// Keeps `self.docker` private to this module (nothing outside
    /// `runs.rs` touches the Docker client directly) while still letting
    /// `effects::docker::observe` discover volume identity without its own
    /// independent Docker-polling loop. Empty (rather than an error) if the
    /// Docker call itself fails — same "purely observational, never worth
    /// surfacing as a hard failure" stance as `refresh`.
    pub async fn volume_names(&self, run_id: &str) -> Vec<String> {
        docker::list_run_volumes(&self.docker, run_id)
            .await
            .unwrap_or_default()
    }

    /// A separate, slower-cadence counterpart to `refresh`, driven by
    /// `daemon::spawn_sync_reconciler` rather than the 1-second liveness
    /// loop: recomputes each live container's *desired* hash from the
    /// current `.fghj.yaml` (`resolve_node_spec(..., side_effects: false)`,
    /// so nothing is actually built, pulled, or run) and compares it against
    /// the hash stamped on the container when it was last actually started
    /// through `fghj`. `graph` is re-resolved by the caller on every tick —
    /// a config-drift check is only meaningful against the *current*
    /// `.fghj.yaml`, not whatever was last cached. Purely informational:
    /// never starts, stops, or recreates anything.
    ///
    /// Reports its verdicts rather than writing them anywhere. The reducer
    /// owns a container's sync status (`state::ContainerObserved::sync`,
    /// via `Action::ConfigDriftObserved`); this used to *also* write it into
    /// a separate `synced` field on its own copy of the container and
    /// re-persist the run, which meant two records of the same fact that
    /// could — and did — disagree.
    pub async fn config_drift(&self, graph: &Graph) -> Vec<DriftReport> {
        let snapshot: Vec<(String, BTreeMap<String, ContainerInfo>)> = {
            let runs = self.runs.lock().unwrap();
            runs.iter()
                .map(|(run_id, state)| (run_id.clone(), state.containers.clone()))
                .collect()
        };

        let mut reports = Vec::new();
        for (run_id, containers) in snapshot {
            for c in containers.values() {
                let synced = match graph.nodes.iter().find(|n| n.id == c.node_id) {
                    Some(node) => match self.resolve_node_spec(graph, node, &run_id, false).await {
                        Ok(Some(spec)) => Some(spec_hash(node, &spec) == c.desired.config_hash),
                        Ok(None) | Err(_) => None,
                    },
                    None => None,
                };
                reports.push(DriftReport {
                    run_id: run_id.clone(),
                    node_id: c.node_id.clone(),
                    synced,
                });
            }
        }
        reports
    }
}

/// One container's config-drift verdict, as of the `Graph` it was computed
/// against. `synced: None` means there was nothing meaningful to compare —
/// either the node is gone from `.fghj.yaml` entirely, or re-resolving its
/// spec failed. Deliberately not `false`, which would read as "confirmed
/// drifted."
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftReport {
    pub run_id: String,
    pub node_id: String,
    pub synced: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::WorkspaceDb;
    use crate::state::{ContainerDesired, PortRoute};
    use std::sync::Arc;

    /// A throwaway container publishing one port to a Docker-picked
    /// ephemeral host port, torn down on drop — exists so `refresh` has a
    /// real container to re-inspect without needing the resolver/`RunOpts`
    /// machinery `start_node` requires.
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

    /// The bug this guards against: a container Docker republished on a new
    /// ephemeral host port (e.g. after a restart-policy-triggered restart,
    /// outside `fghj`'s own start path) used to leave `status` corrected but
    /// `published_port`/`ports` permanently stale, since the old `refresh`
    /// only ever wrote back `status`. Simulates that by seeding the registry
    /// with a deliberately wrong port for a real, running container, then
    /// asserting `refresh` corrects it to the port Docker actually
    /// published. The route pointing at the stale port needs no correcting
    /// here — `state::query::live_host_port` joins it against these
    /// observed ports at read time, which that module's own tests cover.
    #[tokio::test]
    async fn refresh_corrects_observed_ports_after_docker_moves_the_published_port() {
        let container = DriftingPortContainer::start();
        let docker = Arc::new(crate::daemon::connect_docker().expect("docker client"));
        let real_port = docker::inspect_status(&docker, &container.name, "8080")
            .await
            .expect("inspect_status failed")
            .and_then(|s| s.published_port)
            .expect("container must have a real published port");

        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        let registry = RunRegistry::new(tmp.path().to_path_buf(), db, docker)
            .await
            .expect("RunRegistry::new failed");

        let stale_port = real_port.wrapping_add(1).max(1);
        let run_id = "default".to_string();
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
            },
            observed: ContainerObserved {
                status: "running".to_string(),
                published_port: Some(stale_port),
                ports: BTreeMap::from([("8080".to_string(), Some(stale_port))]),
                ..Default::default()
            },
            pending_action: None,
        };
        {
            let mut runs = registry.runs.lock().unwrap();
            runs.insert(
                run_id.clone(),
                RunState {
                    run_id: run_id.clone(),
                    network: "bridge".to_string(),
                    containers: BTreeMap::from([("svc".to_string(), stale_container)]),
                    ..Default::default()
                },
            );
        }

        registry.refresh().await;

        let state = registry.get(&run_id).expect("run must still be tracked");
        let c = &state.containers["svc"];
        assert_eq!(c.observed.published_port, Some(real_port));
        assert_eq!(
            c.observed.ports.get("8080").copied().flatten(),
            Some(real_port)
        );
        // `desired` is untouched by an observation, including the route
        // still recording the port the container was published on when it
        // was started.
        assert!(c.desired.running);
        assert_eq!(c.desired.routes[0].host_port, stale_port);
    }
}
