use std::collections::{HashMap, HashSet};

use anyhow::Result;

use super::health::RunBudget;
use super::naming::{container_name, network_name};
use super::order::{
    StartOutcomes, dependents_of, requirements_of, topological_start_order, waited_on,
};
use super::progress::{NodeDone, ProgressSink, RunProgress, report};
use super::spec::spec_hash;
use super::start_node::StartContext;
use crate::daemon_log;
use crate::docker;
use crate::resolver::{Graph, Node};
use crate::state::{ContainerInfo, EnvCreateError, EnvSpec, EnvState, PendingAction};

use super::registry::Orchestrator;

/// Said, not refused — see `Graph::start_advisories`.
fn log_start_advisories(graph: &Graph, target_ids: &[String]) {
    for advisory in graph.start_advisories(target_ids) {
        eprintln!("fghjd: {advisory}");
    }
}

/// Only `run: once` skips, and only on a clean exit of a container fghj can
/// still see. Not keyed on `config_hash`: `once` marks a task unsafe to re-run.
fn task_already_done(node: &Node, prior: Option<&ContainerInfo>) -> bool {
    node.kind == "task"
        && node.run_policy.as_deref() == Some("once")
        && prior.is_some_and(|c| c.observed.exit_code == Some(0))
}

/// Whether a top-up leaves a node alone: it's alive, fghj has a record of it,
/// and its config hasn't drifted (`None` = unknown, leave it). Drift is acted
/// on here because the user asked; the reconciler never acts on it.
fn top_up_may_skip(
    alive: bool,
    existing: Option<&ContainerInfo>,
    fresh_hash: Option<&str>,
) -> bool {
    alive && existing.is_some_and(|e| fresh_hash.is_none_or(|hash| hash == e.desired.config_hash))
}

/// The first requirement this top-up recreated: `node` may hold connections to
/// it, so restart it too.
fn recreated_requirement<'a>(
    graph: &'a Graph,
    node: &Node,
    recreated: &HashSet<String>,
) -> Option<&'a str> {
    graph
        .edges
        .iter()
        .filter(|e| e.needed_to_start() && e.from == node.id)
        .map(|e| e.to.as_str())
        .find(|to| recreated.contains(*to))
}

/// What a Start of `node_id` brings up: its requirements, itself, and the
/// running services that depend on it.
fn restart_targets(node_id: &str, graph: &Graph, prior: &EnvState) -> Vec<String> {
    let mut ids = requirements_of(node_id, &graph.edges);
    ids.push(node_id.to_string());
    ids.extend(
        dependents_of(node_id, &graph.edges)
            .into_iter()
            .filter(|id| {
                prior.containers.get(id).is_some_and(|c| {
                    c.pending_action.is_none()
                        && !c.desired.terminating
                        && c.observed.status == "running"
                })
            }),
    );
    ids
}

pub struct Restarted {
    pub node: Result<ContainerInfo, String>,
    pub others: Vec<ContainerInfo>,
}

struct BroughtUp {
    state: EnvState,
    outcomes: StartOutcomes,
    started: Vec<String>,
}

impl Orchestrator {
    pub(super) async fn record_blocked(&self, node_id: &str, by: &str) {
        let ev = self.events(node_id, "start");
        ev.begin().await;
        ev.failed(
            "blocked",
            format!("blocked by {by}: it failed, and this can't start without it"),
        )
        .await;
    }

    /// Brings the environment up to date for `spec` without restarting containers
    /// that are alive and current. Switching flow stops what's outside it.
    /// A failed node blocks only its dependents; nothing is rolled back.
    pub async fn ensure_running(
        &self,
        graph: &Graph,
        spec: &EnvSpec,
        prior: Option<&EnvState>,
        progress: Option<&ProgressSink>,
    ) -> Result<EnvState, EnvCreateError> {
        let up = self.bring_up(graph, spec, prior, progress, None).await?;
        match up.outcomes.error() {
            None => Ok(up.state),
            Some(message) => Err(EnvCreateError {
                message,
                partial: Some(Box::new(up.state)),
            }),
        }
    }

    /// Starts `node_id` within the environment — the Drawer's Start button. Always
    /// recreates it, starts requirements only if down, and restarts its running
    /// dependents afterwards.
    pub async fn restart_node(
        &self,
        graph: &Graph,
        node_id: &str,
        prior: &EnvState,
    ) -> Result<Restarted> {
        let spec = EnvSpec::Node(node_id.to_string());
        let up = self
            .bring_up(graph, &spec, Some(prior), None, Some(node_id))
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;
        let others = up
            .started
            .iter()
            .filter(|id| *id != node_id)
            .map(|id| up.state.containers[id].clone())
            .collect();
        let node = if up.started.iter().any(|id| id == node_id) {
            if let Some(message) = up.outcomes.error() {
                daemon_log::warn(format!(
                    "fghjd: {node_id} started, but restarting what depends on it failed: {message}"
                ));
            }
            Ok(up.state.containers[node_id].clone())
        } else {
            Err(up
                .outcomes
                .error()
                .unwrap_or_else(|| format!("{node_id} was not started")))
        };
        Ok(Restarted { node, others })
    }

    /// The one start path behind `ensure_running` and `restart_node`.
    async fn bring_up(
        &self,
        graph: &Graph,
        spec: &EnvSpec,
        prior: Option<&EnvState>,
        progress: Option<&ProgressSink>,
        restart: Option<&str>,
    ) -> Result<BroughtUp, EnvCreateError> {
        let target_ids = match spec {
            EnvSpec::Flow(flow) => graph.start_ids(Some(flow))?,
            EnvSpec::Node(id) => {
                if !graph.nodes.iter().any(|n| &n.id == id) {
                    return Err(anyhow::anyhow!("no such node: {id}").into());
                }
                graph.with_migrations(match (restart, prior) {
                    (Some(_), Some(prior)) => restart_targets(id, graph, prior),
                    _ => {
                        let mut ids = requirements_of(id, &graph.edges);
                        ids.push(id.clone());
                        ids
                    }
                })
            }
        };
        log_start_advisories(graph, &target_ids);
        let network = network_name(&graph.workspace_name);
        docker::ensure_network(&self.docker, &network, &network).await?;
        let (sidecar_container_name, sidecar_ip) =
            self.ensure_sidecar(&graph.workspace_name, &network).await?;

        let mut state = prior.cloned().unwrap_or_default();
        state.network = network.clone();
        state.sidecar_container_name = sidecar_container_name;
        state.sidecar_ip = Some(sidecar_ip);

        if matches!(spec, EnvSpec::Flow(_)) {
            self.stop_outside(graph, &target_ids, &mut state, progress)
                .await;
        }

        let node_map: HashMap<&str, &Node> =
            graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let ordered_ids = topological_start_order(&target_ids, &graph.edges);
        let waited = waited_on(&target_ids, &graph.edges);
        let mut outcomes = StartOutcomes::new(&target_ids, &graph.edges);
        let mut recreated: HashSet<String> = HashSet::new();
        let mut started = Vec::new();
        let budget = RunBudget::default();

        for node_id in &ordered_ids {
            let node = node_map[node_id.as_str()];
            let forced = restart == Some(node_id.as_str());
            let container_name = container_name(&graph.workspace_name, &node.id);
            let existing = state.containers.get(&node.id);
            if !forced {
                // A recreated store starts empty or old, so its migration always runs.
                let store_recreated = node
                    .migrates
                    .as_ref()
                    .is_some_and(|store| recreated.contains(store));
                if restart.is_some() {
                    if existing.is_some_and(|c| {
                        c.pending_action.is_some()
                            || (node.kind == "task"
                                && c.observed.exit_code == Some(0)
                                && !store_recreated)
                    }) {
                        continue;
                    }
                } else if task_already_done(node, existing) {
                    continue;
                }
                if !node.seeds.is_empty() && self.already_seeded(graph, node).await {
                    continue;
                }
            }
            let alive = matches!(
                docker::inspect_status(&self.docker, &container_name).await,
                Ok(Some(s)) if s.status == "running"
            );
            // Alive isn't enough to skip — see `top_up_may_skip`. Resolved only when it
            // could change the answer: it's a graph walk per node.
            let fresh_hash = match (alive, existing) {
                (true, Some(_)) if restart.is_none() => self
                    .resolve_node_spec(graph, node, false)
                    .await
                    .ok()
                    .map(|spec| spec_hash(node, &spec)),
                _ => None,
            };
            let restart_for = recreated_requirement(graph, node, &recreated);
            if !forced
                && restart_for.is_none()
                && top_up_may_skip(alive, existing, fresh_hash.as_deref())
            {
                continue;
            }
            if let Some(by) = outcomes.blocked_by(node_id).map(str::to_string) {
                // An alive container is left as is; only a node this top-up would have
                // started is reported as blocked.
                if !alive {
                    self.record_blocked(node_id, &by).await;
                    outcomes.block(node_id, &by);
                }
                continue;
            }
            if alive && !forced {
                // Narrated: a top-up bouncing a healthy container otherwise looks like a bug.
                let why = match restart_for {
                    Some(dep) => format!(
                        "restarting container: {dep} was recreated, and this can't start \
                         without it"
                    ),
                    None => "recreating container: config changed since it was started".into(),
                };
                self.events(&node.id, "create").ok(&why).await;
            }
            working(progress, node_id, Some(PendingAction::Starting));

            let lock = self.node_lock(node_id);
            let guard = lock.lock().await;
            let started_node = self
                .start_node(
                    graph,
                    node,
                    StartContext {
                        network: &network,
                        sidecar_ip: state.sidecar_ip.as_deref(),
                        budget: &budget,
                        // Kept across a drift recreate so a debug switch isn't silently dropped.
                        debug_wait: existing.is_some_and(|c| c.desired.debug_wait),
                        wait_ready: waited.contains(node_id),
                    },
                )
                .await;
            drop(guard);
            let info = match started_node {
                Ok(info) => info,
                Err(e) => {
                    working(progress, node_id, None);
                    outcomes.fail(node_id, format!("{e:#}"));
                    continue;
                }
            };
            if node.kind != "task" {
                recreated.insert(node.id.clone());
            }
            if !node.seeds.is_empty() {
                self.record_seeded(graph, node).await;
            }
            // Per node, so a crash mid-create doesn't lose track of what started.
            report_done(progress, &state, info.clone());
            state.containers.insert(info.node_id.clone(), info);
            started.push(node_id.clone());
        }

        Ok(BroughtUp {
            state,
            outcomes,
            started,
        })
    }

    /// Stops what's running outside `keep`, dependents first. Best effort.
    async fn stop_outside(
        &self,
        graph: &Graph,
        keep: &[String],
        state: &mut EnvState,
        progress: Option<&ProgressSink>,
    ) {
        let outside: Vec<String> = state
            .containers
            .values()
            .filter(|c| c.observed.status == "running" && !keep.contains(&c.node_id))
            .map(|c| c.node_id.clone())
            .collect();
        for node_id in topological_start_order(&outside, &graph.edges)
            .into_iter()
            .rev()
        {
            let container = state.containers[&node_id].clone();
            working(progress, &node_id, Some(PendingAction::Stopping));
            match self.stop_container(&node_id, &container).await {
                Ok(info) => {
                    report_done(progress, state, info.clone());
                    state.containers.insert(node_id, info);
                }
                Err(e) => {
                    working(progress, &node_id, None);
                    daemon_log::warn(format!(
                        "fghjd: stopping {node_id}, which isn't in the flow, failed: {e:#}"
                    ));
                }
            }
        }
    }
}

fn working(progress: Option<&ProgressSink>, node_id: &str, action: Option<PendingAction>) {
    report(
        progress,
        RunProgress::Working {
            node_id: node_id.to_string(),
            action,
        },
    );
}

fn report_done(progress: Option<&ProgressSink>, state: &EnvState, info: ContainerInfo) {
    report(
        progress,
        RunProgress::Done(Box::new(NodeDone {
            network: state.network.clone(),
            sidecar_container_name: state.sidecar_container_name.clone(),
            sidecar_ip: state.sidecar_ip.clone(),
            info,
        })),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::testing::{edge, runtime_edge, test_graph, test_node};
    use crate::state::{ContainerDesired, ContainerObserved};

    fn task(id: &str, run: &str) -> Node {
        let mut node = test_node(id, id, "task");
        node.run_policy = Some(run.to_string());
        node
    }

    fn finished(exit_code: Option<i64>) -> ContainerInfo {
        ContainerInfo {
            node_id: "migrate.api".into(),
            desired: ContainerDesired {
                terminating: true,
                ..Default::default()
            },
            observed: ContainerObserved {
                status: "exited".into(),
                exit_code,
                ..Default::default()
            },
            pending_action: None,
        }
    }

    fn running(config_hash: &str) -> ContainerInfo {
        ContainerInfo {
            node_id: "api.api".into(),
            desired: ContainerDesired {
                config_hash: config_hash.to_string(),
                ..Default::default()
            },
            observed: ContainerObserved {
                status: "running".into(),
                ..Default::default()
            },
            pending_action: None,
        }
    }

    #[test]
    fn a_running_node_whose_config_is_unchanged_is_left_alone() {
        assert!(top_up_may_skip(true, Some(&running("abc")), Some("abc")));
    }

    #[test]
    fn a_running_node_whose_config_changed_is_recreated() {
        assert!(!top_up_may_skip(true, Some(&running("abc")), Some("def")));
    }

    #[test]
    fn an_inconclusive_hash_leaves_a_running_node_alone() {
        assert!(top_up_may_skip(true, Some(&running("abc")), None));
    }

    #[test]
    fn a_running_node_fghj_has_no_record_of_is_recreated() {
        assert!(!top_up_may_skip(true, None, Some("abc")));
    }

    #[test]
    fn a_dead_node_is_always_recreated() {
        assert!(!top_up_may_skip(false, Some(&running("abc")), Some("abc")));
    }

    #[test]
    fn a_once_task_that_already_exited_zero_is_left_alone() {
        assert!(task_already_done(
            &task("migrate.api", "once"),
            Some(&finished(Some(0)))
        ));
    }

    #[test]
    fn an_on_start_task_runs_again_even_after_a_clean_exit() {
        assert!(!task_already_done(
            &task("migrate.api", "on_start"),
            Some(&finished(Some(0)))
        ));
    }

    /// `once` means "succeed once": a failed run is retried.
    #[test]
    fn a_once_task_that_failed_is_not_treated_as_done() {
        assert!(!task_already_done(
            &task("migrate.api", "once"),
            Some(&finished(Some(3)))
        ));
    }

    #[test]
    fn a_once_task_with_nothing_inherited_runs() {
        assert!(!task_already_done(&task("migrate.api", "once"), None));
    }

    #[test]
    fn a_service_is_never_skipped_however_it_exited() {
        let mut service = test_node("api.shop", "api", "service");
        service.run_policy = Some("once".to_string());
        assert!(!task_already_done(&service, Some(&finished(Some(0)))));
    }

    fn chain() -> Graph {
        test_graph(
            vec![
                test_node("db", "db", "service"),
                test_node("api", "api", "service"),
                test_node("web", "web", "service"),
                test_node("cache", "cache", "service"),
            ],
            vec![
                edge("api", "db", "depends-on"),
                edge("web", "api", "depends-on"),
                runtime_edge("api", "cache"),
            ],
        )
    }

    fn env_with(containers: Vec<ContainerInfo>) -> EnvState {
        crate::state::testing::run_state(containers)
    }

    fn up(node_id: &str) -> ContainerInfo {
        let mut c = crate::state::testing::container(node_id);
        c.observed.status = "running".into();
        c
    }

    #[test]
    fn a_start_brings_requirements_and_restarts_running_dependents() {
        let targets = restart_targets("api", &chain(), &env_with(vec![up("web")]));
        assert_eq!(targets, vec!["db", "api", "web"]);
    }

    #[test]
    fn a_dependent_that_is_not_running_is_left_out() {
        let mut web = up("web");
        web.observed.status = "exited".into();
        let targets = restart_targets("api", &chain(), &env_with(vec![web]));
        assert_eq!(targets, vec!["db", "api"]);
    }

    #[test]
    fn a_dependent_someone_else_is_acting_on_is_left_out() {
        let mut web = up("web");
        web.pending_action = Some(PendingAction::Stopping);
        let targets = restart_targets("api", &chain(), &env_with(vec![web]));
        assert_eq!(targets, vec!["db", "api"]);
    }

    #[test]
    fn a_runtime_edge_pulls_nothing_in() {
        let targets = restart_targets("cache", &chain(), &env_with(vec![up("api")]));
        assert_eq!(targets, vec!["cache"]);
    }
}
