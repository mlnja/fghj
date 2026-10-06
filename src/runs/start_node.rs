//! Creates the Docker container for one node and derives its routes.

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

use super::registry::RunRegistry;

/// Everything about *this* start that isn't the node's own declared config:
/// which run and network the container joins, where the run's sidecar is,
/// how much time the start may spend waiting, and whether the per-container
/// debug switch is on.
///
/// A struct rather than five more positional parameters because `run_id` and
/// `network` are both `&str` and adjacent — a caller that swapped them would
/// compile and then produce a container on the wrong network under the right
/// name.
#[derive(Clone, Copy)]
pub(super) struct StartContext<'a> {
    pub(super) run_id: &'a str,
    pub(super) network: &'a str,
    pub(super) sidecar_ip: Option<&'a str>,
    pub(super) budget: &'a RunBudget,
    /// See [`debug_wait_overrides`] for what this changes and why it is
    /// passed in here rather than read off the node.
    pub(super) debug_wait: bool,
}

impl RunRegistry {
    /// Actually starts a node's container: resolves its full spec via
    /// `resolve_node_spec` (`side_effects: true`, so the image gets built
    /// and named volumes get created along the way), stamps the resulting
    /// `spec_hash` onto the container as a `fghj.config_hash` label, runs
    /// it, and inspects the result. That label — and the copy of the same
    /// hash returned on `ContainerDesired::config_hash` — is what a later
    /// `config_drift` pass compares a freshly recomputed desired hash
    /// against to decide whether this node has drifted since it was last
    /// started.
    pub(super) async fn start_node(
        &self,
        graph: &Graph,
        node: &Node,
        start: StartContext<'_>,
    ) -> Result<ContainerInfo> {
        let StartContext {
            run_id,
            network,
            sidecar_ip,
            budget,
            debug_wait,
        } = start;
        self.begin_event_cycle(run_id, &node.id, "start").await;
        self.record_event(
            run_id,
            &node.id,
            "start",
            "resolving config",
            "running",
            None,
        )
        .await;
        let spec = match self.resolve_node_spec(graph, node, run_id, true).await {
            Ok(Some(spec)) => spec,
            Ok(None) => {
                let msg = format!(
                    "resolve_node_spec returned no spec for {} despite side_effects being enabled",
                    node.id
                );
                self.record_event(
                    run_id,
                    &node.id,
                    "start",
                    "resolving config",
                    "error",
                    Some(msg.clone()),
                )
                .await;
                bail!(msg);
            }
            Err(e) => {
                // The backstop. Most failures in here are a build, a secret
                // or the ssh-agent, and `resolve_node_spec` already recorded
                // the step that actually broke — recording again would put
                // the same text in the events pane a second time under
                // `resolving config`, which is not what failed.
                self.record_error_unless_reported(
                    run_id,
                    &node.id,
                    "start",
                    "resolving config",
                    format!("{e:#}"),
                )
                .await;
                return Err(e);
            }
        };
        self.record_event(run_id, &node.id, "start", "resolving config", "ok", None)
            .await;
        let config_hash = spec_hash(node, &spec);
        let mut labels = node.labels.clone();
        labels.insert("fghj.config_hash".to_string(), config_hash.clone());
        // The same two facts as `ContainerDesired::source`, on the container
        // itself. `config_hash` above is a digest, so `docker inspect` can
        // show that two containers differ but never what either was built
        // from; these make "which commit is this thing running" answerable
        // without fghj, its database, or a resolved graph in hand. Omitted
        // rather than written empty when git could not be read, so a missing
        // label means unknown instead of "no branch".
        if node.build.is_some() {
            if let Some(branch) = node.branch.as_deref() {
                labels.insert("fghj.source_branch".to_string(), branch.to_string());
            }
            if let Some(head) = node.head.as_deref() {
                labels.insert("fghj.source_head".to_string(), head.to_string());
            }
            labels.insert("fghj.source_dirty".to_string(), node.dirty.to_string());
        }

        // Deliberately *after* `spec_hash` above, and that ordering is the
        // whole reason this isn't folded into `resolve_node_spec` — see
        // `debug_wait_overrides`.
        let (env, healthcheck) = debug_wait_overrides(&spec, node, debug_wait);

        // Every node asks this run's sidecar for DNS first — it's the
        // authority for the `fghj.internal` zone (and any active
        // `additional_hosts`/`wildcard_hosts` alias) inside this network,
        // forwarding anything else on to Docker's own embedded resolver.
        // Falls back to Docker's default (unset) only if the sidecar's IP
        // somehow isn't known yet — `ensure_sidecar` always runs, and is
        // inspected for its IP, before any node's `start_node` call, so
        // this shouldn't actually happen in practice.
        let dns: Vec<String> = sidecar_ip
            .map(|ip| vec![ip.to_string(), "127.0.0.11".to_string()])
            .unwrap_or_default();

        self.record_event(
            run_id,
            &node.id,
            "start",
            "creating container",
            "running",
            None,
        )
        .await;
        if let Err(e) = docker::run_container(
            &self.docker,
            &docker::RunOpts {
                name: &spec.container_name,
                network,
                aliases: &spec.aliases,
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
            self.record_event(
                run_id,
                &node.id,
                "start",
                "creating container",
                "error",
                Some(format!("{e:#}")),
            )
            .await;
            return Err(e);
        }
        self.record_event(run_id, &node.id, "start", "creating container", "ok", None)
            .await;

        self.spawn_log_capture(run_id, &node.id, &spec.container_name);

        // Prefer the port explicitly marked `primary` — the one actually
        // meant to be "the" entrypoint — over an arbitrary map-iteration
        // order (a `BTreeMap<String, _>` sorts port numbers as strings, so
        // e.g. "10000" would otherwise sort before "9000").
        let status_port = node
            .ports
            .iter()
            .find(|(_, cfg)| cfg.primary)
            .map(|(port, _)| port.clone())
            .or_else(|| node.ports.keys().next().cloned());
        let inspected = match &status_port {
            Some(p) => docker::inspect_status(&self.docker, &spec.container_name, p).await?,
            None => docker::inspect_status(&self.docker, &spec.container_name, "").await?,
        };
        let (mut status, published_port) = match inspected {
            Some(s) => (s.status, s.published_port),
            None => ("unknown".to_string(), None),
        };
        // Only ever set for a terminating node, and only by the wait below:
        // a service's exit code, if it even has one yet, says nothing this
        // moment after `run_container` returned.
        let mut exit_code: Option<i64> = None;

        // Every declared port's actual host-published binding, not just the
        // routed ones — a plain TCP backing dependency (postgres, mysql)
        // has no `primary`/`name`d port to route at all, but the UI still
        // wants a `127.0.0.1:<port>` connection string for it. Reuses the
        // inspect already done above for `status_port`'s own binding rather
        // than re-querying it; one more inspect per remaining port (there's
        // rarely more than one or two per node).
        //
        // Keyed off `spec.port_list` rather than `node.ports`, because that
        // is the set actually published — it also carries the `debug` port,
        // and `raw_net::reconcile` NATs a node's raw domain only to the
        // ports it finds *here* (via `state::query::raw_endpoints`). Reading
        // `node.ports` instead left the debugger published by Docker but
        // unreachable at `{raw_domain}:{debug}`.
        let mut port_host_ports: BTreeMap<String, Option<u16>> = BTreeMap::new();
        for (port, _) in &spec.port_list {
            let host_port = if status_port.as_deref() == Some(port.as_str()) {
                published_port
            } else {
                docker::inspect_status(&self.docker, &spec.container_name, port)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|s| s.published_port)
            };
            port_host_ports.insert(port.clone(), host_port);
        }

        // Every port with a domain — the `primary` one, at this node's own
        // domain, and/or any `name`d one, at `{name}.{domain}` (a port can
        // be both) — gets a route to the host port Docker actually
        // published it on. `fghjd` runs on the host, not inside the docker
        // network, so it can't resolve these names the way sibling
        // containers do (via Docker's embedded per-network DNS, which only
        // answers from inside that network) — this is what lets
        // `web::proxy::serve_https` dispatch an incoming SNI straight to the
        // right container instead.
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

        // `#AdditionalHost` aliases route to the same host port as the
        // node's own primary domain — same "multiple names, one backend
        // port" pattern as a named port, just keyed off a literal
        // author-declared hostname instead of a derived one. Silently
        // dropped (not an error) if there's no primary-port route to attach
        // to — `resolver::check_ports` already warns about exactly this at
        // graph-resolution time.
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

        // A terminating node is not "started" until it has *finished*: its
        // dependents (the owning service, via the `owns` edge — see
        // `topological_start_order`, for which `to` is always the
        // dependency) come after it in the start order precisely so a
        // migration runs before the service that needs it. Waiting here,
        // and failing the node if the wait doesn't end in a clean exit, is
        // what makes that ordering mean anything: without it "started" would
        // only mean "the container was created", and the service would come
        // up against an unmigrated database.
        //
        // Mutually exclusive with the healthcheck wait below by
        // construction, not just by this `else`: `TaskConfig` has no
        // `healthcheck` field at all, because a container that has exited
        // can never report Docker-healthy.
        if node.kind == "task" {
            self.record_event(
                run_id,
                &node.id,
                "start",
                "waiting for exit",
                "running",
                None,
            )
            .await;
            let allowance = budget.tasks.allowance();
            // Read *before* the wait, not after: waiting until the deadline
            // exhausts the budget by definition, so asking afterwards would
            // report every genuine timeout as "never waited on".
            let spent_before_waiting = budget.tasks.is_exhausted();
            // Unlike the health budget, an exhausted task budget is not a
            // reason to shrug and continue — see `RunBudget` on why the two
            // deadlines are separate.
            let outcome = if spent_before_waiting {
                TaskOutcome::TimedOut
            } else {
                wait_for_exit(&self.docker, &spec.container_name, allowance).await
            };
            // Whatever happened, the container is no longer the "running"
            // that was inspected a moment ago — except on a timeout, where
            // it demonstrably still is.
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
                self.record_event(
                    run_id,
                    &node.id,
                    "start",
                    "waiting for exit",
                    "error",
                    Some(failure.clone()),
                )
                .await;
                bail!("task {} failed: {failure}", node.id);
            }
            self.record_event(run_id, &node.id, "start", "waiting for exit", "ok", None)
                .await;
        } else if node.healthcheck.is_some() {
            self.record_event(
                run_id,
                &node.id,
                "start",
                "waiting for healthcheck",
                "running",
                None,
            )
            .await;
            let allowance = budget.health.allowance();
            // Same reason as the task branch above: a wait that runs to the
            // shared deadline leaves the budget exhausted, so this has to be
            // read before the wait or a real timeout would be reported as a
            // wait that never happened.
            let spent_before_waiting = budget.health.is_exhausted();
            // Once the run's budget is spent there is nothing to wait with,
            // so don't pretend to wait — say so instead of logging a
            // zero-second "gave up".
            let outcome = if spent_before_waiting {
                HealthOutcome::TimedOut
            } else {
                wait_for_healthy(&self.docker, &spec.container_name, allowance).await
            };
            // A wait that ran out of budget is recorded as such rather than
            // as a clean pass. The container is still running and the node
            // still comes up — this is the same best-effort call
            // `wait_for_healthy` has always made at its own limit — but a
            // node that fghj never actually saw report healthy should not
            // look identical in the event stream to one that did.
            let (status, detail) = match outcome {
                HealthOutcome::Healthy => ("ok", None),
                HealthOutcome::Settled => (
                    "ok",
                    Some("container settled without reporting healthy; continuing".to_string()),
                ),
                HealthOutcome::TimedOut if spent_before_waiting => (
                    "ok",
                    Some(
                        "the run's health-wait budget is spent; not waiting on this node"
                            .to_string(),
                    ),
                ),
                HealthOutcome::TimedOut => (
                    "ok",
                    Some(format!(
                        "gave up after {}s without a healthy report; continuing",
                        allowance.as_secs()
                    )),
                ),
            };
            self.record_event(
                run_id,
                &node.id,
                "start",
                "waiting for healthcheck",
                status,
                detail,
            )
            .await;
        }
        self.record_event(run_id, &node.id, "start", "ready", "ok", None)
            .await;

        Ok(ContainerInfo {
            node_id: node.id.clone(),
            desired: ContainerDesired {
                // This call *is* the intent: fghj was just asked to run
                // this container, whatever `status` says about it a
                // millisecond later.
                //
                // Except for a terminating node, whose intent was never for
                // it to be up: it was asked to *run*, which for a task means
                // to finish. Reaching here at all means it exited 0 — the
                // failure paths above all bailed — so `running: true` would
                // describe a container fghj deliberately let stop as
                // drifted, and every projection in `state::query` would keep
                // hunting for a route into it.
                running: node.kind != "task",
                container_name: spec.container_name,
                domain: spec.domain,
                raw_domain: spec.raw_domain,
                routes,
                additional_hosts: additional_hosts_active,
                status_port,
                config_hash,
                // Gated on `node.build` exactly as `spec_hash`'s own
                // `source` is, so the container records a checkout on
                // precisely the nodes whose hash can move because of one.
                source: node.build.as_ref().map(|_| ContainerSource {
                    branch: node.branch.clone(),
                    head: node.head.clone(),
                    dirty: node.dirty,
                }),
                terminating: node.kind == "task",
                debug_wait,
            },
            observed: ContainerObserved {
                status,
                published_port,
                // Nothing here inspects the container's network-internal
                // address; only `Action::ContainerObserved` ever carries one.
                ip: None,
                ports: port_host_ports,
                // Started from the config that was just hashed into
                // `config_hash`, so it cannot have drifted from it yet.
                sync: SyncStatus::Synced,
                exit_code,
            },
            pending_action: None,
        })
    }
}

/// The two things `FGHJ_DEBUG_WAIT` changes about a container, derived from
/// an already-hashed `spec` rather than written into it.
///
/// That ordering is the point. `debug_wait` is an operator action on one
/// container — "halt this one at startup until I attach" — not a statement
/// about the node's config, and it is flipped per container from the UI
/// rather than declared in `.fghj.yaml` (which is committed and shared:
/// pinning it there would block *every* teammate's start of this node
/// forever). Folding it into `spec_hash` would therefore mean
/// `ensure_running`'s drift check (see `orchestrate`'s `top_up_may_skip`)
/// saw a halted container as drifted and recreated it — destroying the debug
/// session the switch just set up — and `config_drift` would report a node
/// as out of sync with a `.fghj.yaml` that never mentioned this.
///
/// Keeping it out of the hash gives the opposite, and correct, behaviour: a
/// top-up leaves a halted container alone, and a recreate for *genuine*
/// config drift drops the flag — which the UI then honestly reports as the
/// switch going off, since the new container really doesn't have it.
///
/// The healthcheck is dropped because a process halted before line 0 can
/// never report Docker-healthy, so leaving it in place would make
/// `wait_for_healthy` burn this node's whole `PER_NODE_LIMIT` and stall
/// every dependent behind a container that is waiting for a human. It is
/// also the honest reading: a halted process genuinely has nothing to say
/// about its own health, so fghj stops asking rather than recording a
/// failure.
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
    use crate::persistence::WorkspaceDb;
    use crate::runs::testing::{test_graph, test_node};

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
        // A container halted before line 0 can never report healthy, so
        // asking would burn the node's whole health budget.
        assert!(
            hc.is_none(),
            "a halted container must not be health-checked"
        );
    }

    /// The invariant the whole design rests on: turning the switch on must
    /// not move the node's `config_hash`, or `ensure_running`'s drift check
    /// would recreate the very container the switch just halted.
    #[test]
    fn the_switch_does_not_move_the_config_hash() {
        let node = test_node("api", "api", "service");
        let spec = debug_spec(vec!["PORT=3000".to_string()]);

        let before = spec_hash(&node, &spec);
        let (env, _) = debug_wait_overrides(&spec, &node, true);

        assert_eq!(before, spec_hash(&node, &spec));
        // ...which only holds because the variable lands in a *new* env, not
        // in the spec that was hashed.
        assert!(env.len() > spec.env.len());
    }

    /// A `RunRegistry` over a throwaway workspace/db plus a real, uniquely
    /// named Docker network, torn down on drop along with every container
    /// this test started on it.
    struct TaskFixture {
        registry: RunRegistry,
        network: String,
        run_id: String,
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
            let registry = RunRegistry::new(tmp.path().to_path_buf(), db, docker);
            TaskFixture {
                registry,
                network,
                // Not "default": a run id folded into the domain keeps two
                // concurrently running test binaries off each other's names.
                run_id: format!("t{nonce}"),
                _tmp: tmp,
            }
        }

        /// A `busybox` task node running `command`. `image` is set, so
        /// nothing here needs an owning service's build — the inheritance
        /// path is the resolver's concern and is covered there.
        fn task(&self, id: &str, command: &[&str]) -> crate::resolver::Node {
            let mut node = test_node(id, id, "task");
            node.image = Some("busybox".to_string());
            node.command = command.iter().map(|s| s.to_string()).collect();
            node.run_policy = Some("on_start".to_string());
            node
        }

        async fn start(&self, node: &crate::resolver::Node) -> Result<crate::state::ContainerInfo> {
            let graph = test_graph(vec![node.clone()], Vec::new());
            self.registry
                .start_node(
                    &graph,
                    node,
                    StartContext {
                        run_id: &self.run_id,
                        network: &self.network,
                        sidecar_ip: None,
                        budget: &super::RunBudget::default(),
                        debug_wait: false,
                    },
                )
                .await
        }

        async fn cleanup(&self, node_id: &str) {
            let name = format!(
                "fghj-shop-{}-{}",
                self.run_id,
                crate::util::label::sanitize_label(node_id)
            );
            docker::stop_and_remove(&self.registry.docker, &name).await;
            docker::remove_network(&self.registry.docker, &self.network).await;
        }
    }

    /// A finished task is not a stopped service. `running` records what fghj
    /// wants to be true at rest, and for a task that is "finished" — so it
    /// must come back `false` even though the start succeeded, or every
    /// projection in `state::query` would treat the exited container as
    /// drift and keep looking for a route into it.
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
        // A task declares no ports, so there is nothing to publish and
        // nothing to route — the absence is what keeps a one-shot container
        // out of DNS and out of the proxy's routing table.
        assert!(info.desired.routes.is_empty());
        assert!(info.desired.status_port.is_none());

        fixture.cleanup("migrate.api").await;
    }

    /// The whole reason the kind exists: a failed migration must fail its
    /// node, because `start`/`ensure_running` stop the start loop on an
    /// error and the owning service comes *after* its task in
    /// `topological_start_order`. Succeeding here would start the service
    /// against an unmigrated database.
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

    /// A task that never exits is the failure `#Task`'s required `command`
    /// exists to prevent, and it must not be waited on forever — the run's
    /// task budget bounds it, and running out is a failure rather than the
    /// shrug a truncated health wait gets.
    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_task_that_never_exits_fails_once_its_budget_runs_out() {
        let fixture = TaskFixture::new().await;
        let node = fixture.task("hangs.api", &["sleep", "60"]);
        let graph = test_graph(vec![node.clone()], Vec::new());
        // Generous on purpose: the budget's clock starts here, not at the
        // wait, so pulling/creating the container spends some of it before
        // `wait_for_exit` is ever reached. Too tight and this would assert
        // the "budget already spent" branch instead of the timeout one.
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
                    run_id: &fixture.run_id,
                    network: &fixture.network,
                    sidecar_ip: None,
                    budget: &budget,
                    debug_wait: false,
                },
            )
            .await
            .expect_err("a task that never exits must fail its node");
        let message = format!("{err:#}");
        assert!(message.contains("still running"), "{message}");

        fixture.cleanup("hangs.api").await;
    }
}
