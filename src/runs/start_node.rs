//! Creates the Docker container for one node and derives its routes.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

use super::health::{HealthBudget, HealthOutcome, wait_for_healthy};
use super::spec::spec_hash;
use crate::dns;
use crate::docker;
use crate::resolver::{Graph, Node};
use crate::state::{ContainerDesired, ContainerInfo, ContainerObserved, PortRoute, SyncStatus};

use super::registry::RunRegistry;

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
        run_id: &str,
        network: &str,
        sidecar_ip: Option<&str>,
        budget: &HealthBudget,
    ) -> Result<ContainerInfo> {
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
                self.record_event(
                    run_id,
                    &node.id,
                    "start",
                    "resolving config",
                    "error",
                    Some(format!("{e:#}")),
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
                env: &spec.env,
                dns: &dns,
                ports: &spec.port_list,
                image: &spec.image,
                command: &node.command,
                project: network,
                service_name: &node.id,
                binds: &spec.binds,
                restart_policy: &node.restart,
                user: node.user.as_deref(),
                working_dir: node.working_dir.as_deref(),
                labels: &labels,
                cap_add: &node.cap_add,
                cap_drop: &node.cap_drop,
                privileged: node.privileged,
                extra_hosts: &node.extra_hosts,
                healthcheck: node.healthcheck.as_ref(),
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
        let (status, published_port) = match inspected {
            Some(s) => (s.status, s.published_port),
            None => ("unknown".to_string(), None),
        };

        // Every declared port's actual host-published binding, not just the
        // routed ones — a plain TCP backing dependency (postgres, mysql)
        // has no `primary`/`name`d port to route at all, but the UI still
        // wants a `127.0.0.1:<port>` connection string for it. Reuses the
        // inspect already done above for `status_port`'s own binding rather
        // than re-querying it; one more inspect per remaining port (there's
        // rarely more than one or two per node).
        let mut port_host_ports: BTreeMap<String, Option<u16>> = BTreeMap::new();
        for port in node.ports.keys() {
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

        if node.healthcheck.is_some() {
            self.record_event(
                run_id,
                &node.id,
                "start",
                "waiting for healthcheck",
                "running",
                None,
            )
            .await;
            let allowance = budget.allowance();
            // Once the run's budget is spent there is nothing to wait with,
            // so don't pretend to wait — say so instead of logging a
            // zero-second "gave up".
            let outcome = if budget.is_exhausted() {
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
                HealthOutcome::TimedOut if budget.is_exhausted() => (
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
                running: true,
                container_name: spec.container_name,
                domain: spec.domain,
                raw_domain: spec.raw_domain,
                routes,
                additional_hosts: additional_hosts_active,
                status_port,
                config_hash,
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
            },
            pending_action: None,
        })
    }
}
