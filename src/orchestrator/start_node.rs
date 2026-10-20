use std::collections::BTreeMap;

use anyhow::{Result, bail};

use super::health::{HealthOutcome, RunBudget, TaskOutcome, wait_for_exit, wait_for_healthy};
use super::spec::spec_hash;
use crate::dns;
use crate::docker;
use crate::resolver::{Graph, Node};
use crate::state::{
    ContainerDesired, ContainerInfo, ContainerObserved, ContainerSource, PortRoute, SyncStatus,
};

use super::events::Events;
use super::registry::Orchestrator;

#[derive(Clone, Copy)]
pub(super) struct StartContext<'a> {
    pub(super) network: &'a str,
    pub(super) sidecar_ip: Option<&'a str>,
    pub(super) budget: &'a RunBudget,
    pub(super) debug_wait: bool,
    /// Whether something in the run needs this node to start, and so waits for its
    /// healthcheck. See `concepts/dependency-kinds.md`.
    pub(super) wait_ready: bool,
}

impl Orchestrator {
    /// Starts a node's container, stamping `spec_hash` on it as `fghj.config_hash`
    /// for drift checks.
    pub(super) async fn start_node(
        &self,
        graph: &Graph,
        node: &Node,
        start: StartContext<'_>,
    ) -> Result<ContainerInfo> {
        let StartContext {
            network,
            sidecar_ip,
            budget,
            debug_wait,
            wait_ready,
        } = start;
        let ev = self.events(&node.id, "start");
        ev.begin().await;
        ev.running("resolving config").await;
        let spec = match self.resolve_node_spec(graph, node, true).await {
            Ok(spec) => spec,
            Err(e) => {
                // `resolve_node_spec` usually records the failing step itself.
                ev.failed_unless_reported("resolving config", &e).await;
                return Err(e);
            }
        };
        ev.ok("resolving config").await;
        let config_hash = spec_hash(node, &spec);
        let mut labels = node.labels.clone();
        labels.insert("fghj.config_hash".to_string(), config_hash.clone());
        // What the container was built from, readable with `docker inspect`. Omitted
        // when git couldn't be read.
        if let Some(checkout) = node.build_checkout() {
            if let Some(branch) = checkout.branch {
                labels.insert("fghj.source_branch".to_string(), branch.to_string());
            }
            if let Some(head) = checkout.head {
                labels.insert("fghj.source_head".to_string(), head.to_string());
            }
            labels.insert("fghj.source_dirty".to_string(), checkout.dirty.to_string());
        }

        // After `spec_hash` on purpose — see `debug_wait_overrides`.
        let (env, healthcheck) = debug_wait_overrides(&spec, node, debug_wait);

        // The sidecar answers `fghj.internal` and forwards the rest to Docker's DNS.
        let dns: Vec<String> = sidecar_ip
            .map(|ip| vec![ip.to_string(), "127.0.0.11".to_string()])
            .unwrap_or_default();

        ev.running("creating container").await;
        if let Err(e) = docker::run_container(
            &self.docker,
            &docker::RunOpts {
                name: &spec.container_name,
                network,
                aliases: &spec.aliases,
                ipv4_address: None,
                env: &env,
                dns: &dns,
                ports: &spec.port_list,
                image: &spec.image,
                command: &node.command,
                project: network,
                service_name: &node.id,
                binds: &spec.binds,
                restart_policy: &node.restart,
                stop_signal: node.stop_signal.as_deref(),
                stop_grace_period: node.stop_grace_period,
                user: node.user.as_deref(),
                working_dir: node.working_dir.as_deref(),
                labels: &labels,
                cap_add: &node.cap_add,
                cap_drop: &node.cap_drop,
                privileged: node.privileged,
                extra_hosts: &node.extra_hosts,
                healthcheck,
                platform: node.platform.as_deref(),
            },
        )
        .await
        {
            ev.failed("creating container", &e).await;
            return Err(e);
        }
        ev.ok("creating container").await;

        // From the start: a new container has nothing older to skip, and a task
        // can print and exit before a stream from "now" is attached.
        self.spawn_log_capture(&node.id, &spec.container_name, docker::LogsFrom::Since(0));

        // Prefer `primary`: the map sorts ports as strings ("10000" < "9000").
        let status_port = node
            .ports
            .iter()
            .find(|(_, cfg)| cfg.primary)
            .map(|(port, _)| port.clone())
            .or_else(|| node.ports.keys().next().cloned());
        let inspected = docker::inspect_status(&self.docker, &spec.container_name).await?;
        let host_port = |port: &str| inspected.as_ref().and_then(|s| s.host_port(port));
        let published_port = status_port.as_deref().and_then(host_port);
        // Every published port, routed or not: the UI shows a connection string for
        // plain TCP services, and raw_net NATs exactly these. Keyed off
        // `spec.port_list` so the `debug` port is included.
        let port_host_ports: BTreeMap<String, Option<u16>> = spec
            .port_list
            .iter()
            .map(|(port, _)| (port.clone(), host_port(port)))
            .collect();
        let mut status = inspected.map_or_else(|| "unknown".to_string(), |s| s.status);
        let mut exit_code: Option<i64> = None;

        // `fghjd` runs on the host and can't use Docker's DNS, so each domain routes
        // to the host port Docker published.
        let mut routes = Vec::new();
        for (port, cfg) in &node.ports {
            if !cfg.primary && cfg.name.is_none() {
                continue;
            }
            let Some(host_port) = port_host_ports.get(port).copied().flatten() else {
                continue;
            };
            if cfg.primary {
                routes.push(PortRoute {
                    https: dns::cert_eligible(&spec.domain, true),
                    domain: spec.domain.clone(),
                    host_port,
                    wildcard: cfg.wildcard,
                    container_port: port.clone(),
                });
            }
            if let Some(name) = &cfg.name {
                let domain = format!("{name}.{}", spec.domain);
                routes.push(PortRoute {
                    https: dns::cert_eligible(&domain, true),
                    domain,
                    host_port,
                    wildcard: cfg.wildcard,
                    container_port: port.clone(),
                });
            }
        }

        // `#AdditionalHost` aliases share the primary domain's route; dropped without
        // one (the resolver already warned).
        let mut additional_hosts_active = Vec::new();
        if let Some((host_port, container_port)) = routes
            .iter()
            .find(|r| r.domain == spec.domain)
            .map(|r| (r.host_port, r.container_port.clone()))
        {
            for host in &node.additional_hosts {
                routes.push(PortRoute {
                    https: dns::cert_eligible(host, true),
                    domain: host.clone(),
                    host_port,
                    wildcard: false,
                    container_port: container_port.clone(),
                });
                additional_hosts_active.push(host.clone());
            }
            for suffix in &node.wildcard_hosts {
                routes.push(PortRoute {
                    https: dns::cert_eligible(suffix, true),
                    domain: suffix.clone(),
                    host_port,
                    wildcard: true,
                    container_port: container_port.clone(),
                });
            }
        }

        // A task isn't started until it exits cleanly: dependents come after it, and
        // must not start against, say, an unmigrated database.
        if node.kind == "task" {
            ev.running("waiting for exit").await;
            let allowance = budget.tasks.allowance();
            // Read before the wait: waiting to the deadline exhausts the budget.
            let spent_before_waiting = budget.tasks.is_exhausted();
            let outcome = if spent_before_waiting {
                TaskOutcome::TimedOut
            } else {
                wait_for_exit(&self.docker, &spec.container_name, allowance).await
            };
            if outcome != TaskOutcome::TimedOut {
                status = "exited".to_string();
            }
            let failure = match outcome {
                TaskOutcome::Completed => {
                    exit_code = Some(0);
                    None
                }
                TaskOutcome::Failed { exit_code: code } => {
                    exit_code = code;
                    Some(match code {
                        Some(code) => format!("task exited with code {code}"),
                        None => {
                            "task container disappeared before it reported an exit code".to_string()
                        }
                    })
                }
                TaskOutcome::TimedOut if spent_before_waiting => Some(
                    "the run's task budget is spent; this task was never waited on".to_string(),
                ),
                TaskOutcome::TimedOut => Some(format!(
                    "task was still running after {}s; a task is expected to exit",
                    allowance.as_secs()
                )),
            };
            if let Some(failure) = failure {
                ev.failed("waiting for exit", &failure).await;
                bail!("task {} failed: {failure}", node.id);
            }
            ev.ok("waiting for exit").await;
        } else if node.healthcheck.is_some() && wait_ready {
            self.wait_healthy(&ev, &spec.container_name, budget).await;
        }
        ev.ok("ready").await;

        Ok(ContainerInfo {
            node_id: node.id.clone(),
            desired: ContainerDesired {
                // A task's intent is to finish, so `running: false` once it has.
                running: node.kind != "task",
                container_name: spec.container_name,
                domain: spec.domain,
                raw_domain: spec.raw_domain,
                routes,
                additional_hosts: additional_hosts_active,
                status_port,
                config_hash,
                source: node.build_checkout().map(|c| ContainerSource {
                    branch: c.branch.map(str::to_string),
                    head: c.head.map(str::to_string),
                    dirty: c.dirty,
                }),
                terminating: node.kind == "task",
                debug_wait,
            },
            observed: ContainerObserved {
                status,
                published_port,
                ports: port_host_ports,
                sync: SyncStatus::Synced,
                exit_code,
                interrupted: false,
                unseen: false,
            },
            pending_action: None,
        })
    }
}

impl Orchestrator {
    /// `docker restart` of a running container, for a dependent whose
    /// requirement was just recreated or restarted: it may hold connections to
    /// the old one.
    /// Not a recreate, so it keeps its image and config, and the code it runs
    /// is the code it ran.
    pub(super) async fn restart_in_place(
        &self,
        node: &Node,
        existing: &ContainerInfo,
        why: &str,
        budget: &RunBudget,
        wait_ready: bool,
    ) -> Result<ContainerInfo> {
        let name = existing.desired.container_name.as_str();
        let ev = self.events(&node.id, "start");
        ev.begin().await;
        ev.record("restarting container", "running", Some(why.to_string()))
            .await;
        let restarted_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i32);
        if let Err(e) = docker::restart_container(&self.docker, name).await {
            ev.failed("restarting container", &e).await;
            return Err(e);
        }
        ev.ok("restarting container").await;
        // The old log stream ended with the old process. Not from the start:
        // that output is already in the previous generation.
        self.spawn_log_capture(&node.id, name, docker::LogsFrom::Since(restarted_at));
        if node.healthcheck.is_some() && wait_ready && !existing.desired.debug_wait {
            self.wait_healthy(&ev, name, budget).await;
        }
        ev.ok("ready").await;

        // Docker may hand out new ephemeral host ports on a restart.
        let inspected = docker::inspect_status(&self.docker, name).await?;
        let mut info = existing.clone();
        info.pending_action = None;
        if let Some(s) = inspected {
            for (port, host_port) in info.observed.ports.iter_mut() {
                *host_port = s.host_port(port);
            }
            for route in &mut info.desired.routes {
                if let Some(p) = s.host_port(&route.container_port) {
                    route.host_port = p;
                }
            }
            if let Some(port) = &info.desired.status_port {
                info.observed.published_port = s.host_port(port);
            }
            info.observed.status = s.status;
        }
        Ok(info)
    }

    async fn wait_healthy(&self, ev: &Events<'_>, container_name: &str, budget: &RunBudget) {
        ev.running("waiting for healthcheck").await;
        let allowance = budget.health.allowance();
        let spent_before_waiting = budget.health.is_exhausted();
        let outcome = if spent_before_waiting {
            HealthOutcome::TimedOut
        } else {
            wait_for_healthy(&self.docker, container_name, allowance).await
        };
        // A truncated wait is recorded as such, not as a clean pass.
        let detail = match outcome {
            HealthOutcome::Healthy => None,
            HealthOutcome::Settled => {
                Some("container settled without reporting healthy; continuing".to_string())
            }
            HealthOutcome::TimedOut if spent_before_waiting => {
                Some("the run's health-wait budget is spent; not waiting on this node".to_string())
            }
            HealthOutcome::TimedOut => Some(format!(
                "gave up after {}s without a healthy report; continuing",
                allowance.as_secs()
            )),
        };
        ev.record("waiting for healthcheck", "ok", detail).await;
    }
}

/// `FGHJ_DEBUG_WAIT`'s changes, applied after `spec_hash`. It's an operator
/// switch on one container, not config: hashing it would make a top-up see the
/// halted container as drifted and recreate it. The healthcheck is dropped
/// because a halted process can never report healthy.
fn debug_wait_overrides<'a>(
    spec: &super::spec::NodeSpec,
    node: &'a Node,
    debug_wait: bool,
) -> (Vec<String>, Option<&'a crate::resolver::Healthcheck>) {
    if !debug_wait {
        return (spec.env.clone(), node.healthcheck.as_ref());
    }
    let mut env = spec.env.clone();
    env.push("FGHJ_DEBUG_WAIT=1".to_string());
    (env, None)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::orchestrator::testing::{test_graph, test_node};
    use crate::persistence::WorkspaceDb;

    fn debug_spec(env: Vec<String>) -> super::super::spec::NodeSpec {
        super::super::spec::NodeSpec {
            container_name: "c".to_string(),
            domain: "d".to_string(),
            raw_domain: "d".to_string(),
            aliases: Vec::new(),
            image: "busybox".to_string(),
            port_list: vec![("9229".to_string(), None)],
            binds: Vec::new(),
            env,
        }
    }

    fn healthcheck() -> crate::resolver::Healthcheck {
        crate::resolver::Healthcheck {
            test: vec!["CMD".to_string(), "true".to_string()],
            interval: None,
            timeout: None,
            retries: None,
            start_period: None,
        }
    }

    #[test]
    fn the_switch_off_changes_nothing_about_the_container() {
        let mut node = test_node("api", "api", "service");
        node.healthcheck = Some(healthcheck());
        let spec = debug_spec(vec!["PORT=3000".to_string()]);

        let (env, hc) = debug_wait_overrides(&spec, &node, false);

        assert_eq!(env, spec.env);
        assert!(hc.is_some());
    }

    #[test]
    fn the_switch_on_adds_the_wait_variable_and_drops_the_healthcheck() {
        let mut node = test_node("api", "api", "service");
        node.healthcheck = Some(healthcheck());
        let spec = debug_spec(vec!["PORT=3000".to_string()]);

        let (env, hc) = debug_wait_overrides(&spec, &node, true);

        assert_eq!(env.last().map(String::as_str), Some("FGHJ_DEBUG_WAIT=1"));
        assert!(
            hc.is_none(),
            "a halted container must not be health-checked"
        );
    }

    #[test]
    fn the_switch_does_not_move_the_config_hash() {
        let node = test_node("api", "api", "service");
        let spec = debug_spec(vec!["PORT=3000".to_string()]);

        let before = spec_hash(&node, &spec);
        let (env, _) = debug_wait_overrides(&spec, &node, true);

        assert_eq!(before, spec_hash(&node, &spec));
        assert!(env.len() > spec.env.len());
    }

    struct TaskFixture {
        registry: Orchestrator,
        network: String,
        workspace: String,
        _tmp: tempfile::TempDir,
    }

    impl TaskFixture {
        async fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let network = format!("fghj-task-test-{nonce}");
            let tmp = tempfile::tempdir().unwrap();
            let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
            let docker = Arc::new(crate::daemon::connect_docker().expect("docker client"));
            docker::ensure_network(&docker, &network, &network)
                .await
                .expect("ensure_network");
            let registry = Orchestrator::new(tmp.path().to_path_buf(), db, docker);
            TaskFixture {
                registry,
                network,
                workspace: format!("t{nonce}"),
                _tmp: tmp,
            }
        }

        fn task(&self, id: &str, command: &[&str]) -> crate::resolver::Node {
            let mut node = test_node(id, id, "task");
            node.image = Some("busybox".to_string());
            node.command = command.iter().map(|s| s.to_string()).collect();
            node.run_policy = Some("on_start".to_string());
            node
        }

        async fn start(&self, node: &crate::resolver::Node) -> Result<crate::state::ContainerInfo> {
            let graph = self.graph(node);
            self.registry
                .start_node(
                    &graph,
                    node,
                    StartContext {
                        network: &self.network,
                        sidecar_ip: None,
                        budget: &super::RunBudget::default(),
                        debug_wait: false,
                        wait_ready: true,
                    },
                )
                .await
        }

        fn graph(&self, node: &crate::resolver::Node) -> crate::resolver::Graph {
            let mut graph = test_graph(vec![node.clone()], Vec::new());
            graph.workspace_name = self.workspace.clone();
            graph
        }

        async fn cleanup(&self, node_id: &str) {
            let name = crate::orchestrator::naming::container_name(&self.workspace, node_id);
            docker::stop_and_remove(&self.registry.docker, &name).await;
            docker::remove_network(&self.registry.docker, &self.network).await;
        }
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_task_that_exits_zero_comes_back_finished_not_running() {
        let fixture = TaskFixture::new().await;
        let node = fixture.task("migrate.api", &["true"]);
        let info = fixture.start(&node).await.expect("task should succeed");

        assert!(info.desired.terminating);
        assert!(!info.desired.running);
        assert_eq!(info.observed.status, "exited");
        assert_eq!(info.observed.exit_code, Some(0));
        assert!(info.desired.routes.is_empty());
        assert!(info.desired.status_port.is_none());

        fixture.cleanup("migrate.api").await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_task_that_exits_nonzero_fails_the_node() {
        let fixture = TaskFixture::new().await;
        let node = fixture.task("migrate.api", &["sh", "-c", "exit 3"]);
        let err = fixture
            .start(&node)
            .await
            .expect_err("a task exiting 3 must not report a healthy start");
        let message = format!("{err:#}");
        assert!(message.contains("exited with code 3"), "{message}");

        fixture.cleanup("migrate.api").await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_task_that_never_exits_fails_once_its_budget_runs_out() {
        let fixture = TaskFixture::new().await;
        let node = fixture.task("hangs.api", &["sleep", "60"]);
        let graph = fixture.graph(&node);
        // Generous: the budget's clock starts before the container is created.
        let budget = super::RunBudget {
            health: super::super::health::HealthBudget::default(),
            tasks: super::super::health::HealthBudget::new(std::time::Duration::from_secs(12)),
        };
        let err = fixture
            .registry
            .start_node(
                &graph,
                &node,
                StartContext {
                    network: &fixture.network,
                    sidecar_ip: None,
                    budget: &budget,
                    debug_wait: false,
                    wait_ready: true,
                },
            )
            .await
            .expect_err("a task that never exits must fail its node");
        let message = format!("{err:#}");
        assert!(message.contains("still running"), "{message}");

        fixture.cleanup("hangs.api").await;
    }

    /// A task can print and exit before a stream from "now" is attached; a
    /// silent-looking migration was one that finished in 150ms.
    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_task_that_exits_at_once_still_has_its_output_captured() {
        let fixture = TaskFixture::new().await;
        let node = fixture.task(
            "migrate.api",
            &["sh", "-c", "echo applied 3; echo warn >&2"],
        );
        fixture.start(&node).await.expect("task should succeed");

        let db = fixture.registry.db.clone();
        let mut lines = Vec::new();
        for _ in 0..50 {
            let generations = db
                .clone()
                .list_log_generations("migrate.api".into())
                .await
                .unwrap();
            if let Some(g) = generations.iter().max_by_key(|g| g.generation)
                && g.line_count >= 2
            {
                lines = db
                    .clone()
                    .load_log_history("migrate.api".into(), g.generation, None, 10)
                    .await
                    .unwrap();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let text: Vec<_> = lines
            .iter()
            .map(|l| (l.stream.as_str(), l.line.as_str()))
            .collect();

        assert!(text.contains(&("stdout", "applied 3")), "{text:?}");
        assert!(text.contains(&("stderr", "warn")), "{text:?}");

        fixture.cleanup("migrate.api").await;
    }

    fn inspect(name: &str, format: &str) -> String {
        let out = std::process::Command::new("docker")
            .args(["inspect", "--format", format, name])
            .output()
            .expect("failed to run docker");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_restart_in_place_keeps_the_container_and_starts_it_again() {
        let fixture = TaskFixture::new().await;
        let mut node = test_node("php.api", "php", "service");
        node.image = Some("busybox".to_string());
        node.command = vec!["sleep".to_string(), "60".to_string()];
        let before = fixture.start(&node).await.expect("service should start");
        let name = before.desired.container_name.clone();
        let id = inspect(&name, "{{.Id}}");
        let started = inspect(&name, "{{.State.StartedAt}}");

        let after = fixture
            .registry
            .restart_in_place(
                &node,
                &before,
                "mysql.api was recreated",
                &super::RunBudget::default(),
                true,
            )
            .await
            .expect("restart should succeed");

        assert_eq!(inspect(&name, "{{.Id}}"), id, "the same container");
        assert_ne!(
            inspect(&name, "{{.State.StartedAt}}"),
            started,
            "started again"
        );
        assert_eq!(after.observed.status, "running");
        assert_eq!(after.desired.config_hash, before.desired.config_hash);
        assert!(after.pending_action.is_none());

        fixture.cleanup("php.api").await;
    }
}
