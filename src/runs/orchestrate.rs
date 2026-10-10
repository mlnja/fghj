//! Whole-run orchestration: starting a run and bringing it up to date.

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use super::health::RunBudget;
use super::naming::DEFAULT_RUN_ID;
use super::order::{StartOutcomes, requirements_of, topological_start_order, waited_on};
use super::progress::{NodeDone, ProgressSink, RunProgress, report};
use super::spec::spec_hash;
use super::start_node::StartContext;
use crate::daemon_log;
use crate::docker;
use crate::resolver::{Graph, Node};
use crate::state::{ContainerInfo, PendingAction, RunCreateError, RunSpec, RunState};
use crate::util::label::sanitize_label;

use super::registry::RunRegistry;

/// Whether a top-up should leave `node` alone because it is a terminating
/// node that has already done its job.
///
/// `prior` is whatever this top-up inherited for the node, if anything.
///
/// Only ever true for `run: once`. The default, `on_start`, re-runs a task
/// on every start and every top-up — which is why a task's command is
/// required to be idempotent, and why `once` is the opt-in rather than the
/// other way round: skipping is the answer that can be silently wrong.
///
/// The evidence for "already done" is deliberately the container itself and
/// nothing else, *not* `config_hash`. The hash is now commit-aware (it covers
/// the checkout's HEAD, so a `git pull` does move it — `concepts/AUDIT.md`
/// B13), which means a hash-keyed `once` would be implementable. It is still
/// not wanted: `once` means "at most once per run", and a task is `once`
/// precisely when re-running it is expensive or destructive. "New code
/// arrived" is not a reason to re-run something the author marked as unsafe
/// to re-run — if it were, they would have left it `on_start`.
///
/// And the evidence is a container fghj can still see:
/// `persistence::rehydrate` drops any container Docker no longer has, so a
/// task whose container was removed out-of-band runs again rather than being
/// assumed finished on the strength of a record of it.
/// Said, not refused — see `Graph::start_advisories`.
fn log_start_advisories(graph: &Graph, target_ids: &[String]) {
    for advisory in graph.start_advisories(target_ids) {
        eprintln!("fghjd: {advisory}");
    }
}

fn task_already_done(node: &Node, prior: Option<&ContainerInfo>) -> bool {
    node.kind == "task"
        && node.run_policy.as_deref() == Some("once")
        && prior.is_some_and(|c| c.observed.exit_code == Some(0))
}

/// Whether a top-up should leave an already-running node completely alone.
///
/// Three conditions, and all three have to hold:
///
/// 1. **The container is alive.** Checked against Docker itself, not against
///    the persisted `RunState`, since a container can be stopped or removed
///    out-of-band between calls.
/// 2. **fghj still has an `ContainerInfo` for it.** `state.containers` is
///    loaded from the DB (see `RunRegistry::new`'s reconciliation, which
///    drops a whole run's history the moment any one of its containers isn't
///    found), so it can be missing a node whose container is running
///    perfectly well. Recreating is how such a node gets re-described and
///    its routes re-registered, instead of staying silently unrouted until
///    something else happens to bounce it.
/// 3. **Its config has not drifted.** `fresh_hash` is `spec_hash` recomputed
///    from the current graph; `None` means re-resolving was inconclusive, in
///    which case the container is left alone — nothing has established it is
///    wrong, and bouncing a healthy container on a guess is the more
///    expensive of the two mistakes.
///
/// Condition 3 is the one that makes a top-up mean what a user expects. A
/// top-up is an explicit action (switching to a flow), and the mental model people bring to it is `docker compose
/// up`'s: my edits take effect. Before this, a container flagged `Drifted`
/// was skipped precisely *because* it was alive, so editing `.fghj.yaml` and
/// pressing the button did nothing at all — [[config-drift]]'s "observe but
/// never act" policy leaking out of the reconciler, where it belongs, into a
/// path where the user had asked.
///
/// The reconciler still never acts on drift, and that asymmetry is
/// deliberate: a `git switch` changes the graph under a live environment
/// constantly, and a background loop recreating containers in response would
/// fight whoever is working in it. The question was never "should drift be
/// healed" but *who asked*.
fn top_up_may_skip(
    alive: bool,
    existing: Option<&ContainerInfo>,
    fresh_hash: Option<&str>,
) -> bool {
    let Some(existing) = existing else {
        return false;
    };
    alive && fresh_hash.is_none_or(|hash| hash == existing.desired.config_hash)
}

/// The first of `node`'s requirements that this top-up recreated, if any —
/// a reason to restart `node` even though it's alive and unchanged: it may
/// be holding connections to the container that just went away.
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

impl RunRegistry {
    /// Narrates a node a start skipped because something it can't start
    /// without failed.
    pub(super) async fn record_blocked(&self, run_id: &str, node_id: &str, by: &str) {
        self.begin_event_cycle(run_id, node_id, "start").await;
        self.record_event(
            run_id,
            node_id,
            "start",
            "blocked",
            "error",
            Some(format!(
                "blocked by {by}: it failed, and this can't start without it"
            )),
        )
        .await;
    }

    /// Brings the workspace's environment up to date for `spec` — creating
    /// its network and sidecar if they don't exist yet — without
    /// restarting a container that's already alive and current.
    ///
    /// Switching to a flow first stops every running container outside it,
    /// dependents before what they depend on. Stopped, not removed:
    /// switching back starts the same containers on the same volumes.
    /// Starting a single node stops nothing.
    ///
    /// Liveness is checked directly against docker on every call rather than
    /// trusting the persisted `RunState`, since a container can be
    /// stopped/removed out-of-band between calls (see `refresh`).
    ///
    /// A node that fails blocks what can't start without it, and nothing
    /// else, exactly as in `start`. Tearing down the three containers that
    /// came up because the fourth didn't would destroy exactly the progress
    /// the per-node reporting below exists to keep. Each node is reported
    /// through `progress` as it comes up, and the error additionally carries
    /// the whole partial state (`RunCreateError::partial`), so the
    /// containers stay visible and routable instead of running unseen.
    ///
    /// A node this top-up recreates also restarts the alive nodes that
    /// can't start without it, after it is ready again. A task doesn't
    /// count: `on_start` tasks re-run on every top-up, and bouncing
    /// everything behind them each time would make a top-up useless.
    pub async fn ensure_running(
        &self,
        graph: &Graph,
        spec: &RunSpec,
        prior: Option<&RunState>,
        progress: Option<&ProgressSink>,
    ) -> Result<RunState, RunCreateError> {
        let run_id = DEFAULT_RUN_ID.to_string();
        let target_ids = match spec {
            RunSpec::Flow(flow) => graph.start_ids(Some(flow))?,
            RunSpec::Node(id) => {
                if !graph.nodes.iter().any(|n| &n.id == id) {
                    return Err(anyhow::anyhow!("no such node: {id}").into());
                }
                let mut ids = requirements_of(id, &graph.edges);
                ids.push(id.clone());
                ids
            }
        };
        log_start_advisories(graph, &target_ids);
        let network = format!("fghj-{}-{}", sanitize_label(&graph.workspace_name), run_id);
        docker::ensure_network(&self.docker, &network, &network).await?;
        let (sidecar_container_name, sidecar_ip) = self
            .ensure_sidecar(&graph.workspace_name, &run_id, &network)
            .await?;

        let mut state = prior.cloned().unwrap_or_else(|| RunState {
            run_id: run_id.clone(),
            network: network.clone(),
            sidecar_container_name: sidecar_container_name.clone(),
            sidecar_ip: Some(sidecar_ip.clone()),
            ..Default::default()
        });
        state.network = network.clone();
        state.sidecar_container_name = sidecar_container_name;
        state.sidecar_ip = Some(sidecar_ip);

        if matches!(spec, RunSpec::Flow(_)) {
            self.stop_outside(&run_id, graph, &target_ids, &mut state, progress)
                .await;
        }

        let node_map: HashMap<&str, &Node> =
            graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let ordered_ids = topological_start_order(&target_ids, &graph.edges);
        let waited = waited_on(&target_ids, &graph.edges);
        let mut outcomes = StartOutcomes::new(&target_ids, &graph.edges);
        // Services (not tasks) this top-up (re)started.
        let mut recreated: HashSet<String> = HashSet::new();
        let budget = RunBudget::default();

        for node_id in &ordered_ids {
            let node = node_map[node_id.as_str()];
            let container_name = format!(
                "fghj-{}-{}-{}",
                sanitize_label(&graph.workspace_name),
                run_id,
                sanitize_label(&node.id)
            );
            // A terminating node is never "running" once it has done its
            // job, so the liveness check below says nothing useful about
            // whether it needs to run again — `run` does. `on_start` (the
            // default) falls through and re-runs it every top-up, which is
            // why a task's command is required to be idempotent; `once`
            // skips it, but only on the evidence of a clean exit fghj can
            // still see. That evidence is the container itself: `rehydrate`
            // drops any container Docker no longer has, so a task whose
            // container was removed out-of-band runs again rather than being
            // assumed done.
            //
            // Deliberately not keyed on `config_hash` — see
            // `task_already_done` for why, now that the hash can in fact see
            // a new commit.
            if task_already_done(node, state.containers.get(&node.id)) {
                continue;
            }
            let alive = matches!(
                docker::inspect_status(&self.docker, &container_name, "").await,
                Ok(Some(s)) if s.status == "running"
            );
            // Alive alone isn't enough to skip — see `top_up_may_skip` for
            // the three conditions and why drift is one of them here but not
            // in the reconciler.
            let existing = state.containers.get(&node.id);
            // Only resolved when it could change the answer — this is a
            // whole graph walk per node otherwise.
            let fresh_hash = match (alive, existing) {
                (true, Some(_)) => {
                    match self.resolve_node_spec(graph, node, &run_id, false).await {
                        Ok(Some(spec)) => Some(spec_hash(node, &spec)),
                        // Inconclusive. Re-resolving failed, so nothing here can
                        // say the running container is wrong.
                        Ok(None) | Err(_) => None,
                    }
                }
                _ => None,
            };
            let restart_for = recreated_requirement(graph, node, &recreated);
            if restart_for.is_none() && top_up_may_skip(alive, existing, fresh_hash.as_deref()) {
                continue;
            }
            if let Some(by) = outcomes.blocked_by(node_id).map(str::to_string) {
                // An alive container is left as it is: replacing it with
                // nothing helps no one. Only a node this top-up would have
                // started is reported as blocked.
                if !alive {
                    self.record_blocked(&run_id, node_id, &by).await;
                    outcomes.block(node_id, &by);
                }
                continue;
            }
            if alive {
                // Narrated, because from the outside a top-up bouncing
                // something that was running fine looks like a bug.
                let why = match restart_for {
                    Some(dep) => format!(
                        "restarting container: {dep} was recreated, and this can't start \
                         without it"
                    ),
                    None => "recreating container: config changed since it was started".into(),
                };
                self.record_event(&run_id, &node.id, "create", &why, "ok", None)
                    .await;
            }
            working(progress, &run_id, node_id, Some(PendingAction::Starting));

            let info = match self
                .start_node(
                    graph,
                    node,
                    StartContext {
                        run_id: &run_id,
                        network: &network,
                        sidecar_ip: state.sidecar_ip.as_deref(),
                        budget: &budget,
                        // Preserved across a drift recreate, so a top-up
                        // that rebuilds this node for a genuine
                        // `.fghj.yaml` change doesn't silently drop a debug
                        // switch someone has on.
                        debug_wait: existing.is_some_and(|c| c.desired.debug_wait),
                        wait_ready: waited.contains(node_id),
                    },
                )
                .await
            {
                Ok(info) => info,
                Err(e) => {
                    working(progress, &run_id, node_id, None);
                    outcomes.fail(node_id, format!("{e:#}"));
                    continue;
                }
            };
            if node.kind != "task" {
                recreated.insert(node.id.clone());
            }
            // Reported after every node, not just at the end, so a later
            // failure — or a daemon that dies outright — doesn't lose track
            // of containers that did start successfully.
            report_done(progress, &state, info.clone());
            state.containers.insert(info.node_id.clone(), info);
        }

        match outcomes.error() {
            None => Ok(state),
            Some(message) => Err(RunCreateError {
                message,
                partial: Some(Box::new(state)),
            }),
        }
    }

    /// The other half of switching to a flow: stops every running container
    /// in `state` that isn't one of `keep`, in reverse start order so
    /// nothing is left running without what it needs. Best effort — a
    /// container that won't stop is logged and left, since failing the
    /// switch over it would also leave the flow itself not started.
    async fn stop_outside(
        &self,
        run_id: &str,
        graph: &Graph,
        keep: &[String],
        state: &mut RunState,
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
            working(progress, run_id, &node_id, Some(PendingAction::Stopping));
            match self.stop_container(run_id, &node_id, &container).await {
                Ok(info) => {
                    report_done(progress, state, info.clone());
                    state.containers.insert(node_id, info);
                }
                Err(e) => {
                    working(progress, run_id, &node_id, None);
                    daemon_log::warn(format!(
                        "fghjd: stopping {node_id}, which isn't in the flow, failed: {e:#}"
                    ));
                }
            }
        }
    }
}

fn working(
    progress: Option<&ProgressSink>,
    run_id: &str,
    node_id: &str,
    action: Option<PendingAction>,
) {
    report(
        progress,
        RunProgress::Working {
            run_id: run_id.to_string(),
            node_id: node_id.to_string(),
            action,
        },
    );
}

fn report_done(progress: Option<&ProgressSink>, state: &RunState, info: ContainerInfo) {
    report(
        progress,
        RunProgress::Done(Box::new(NodeDone {
            run_id: state.run_id.clone(),
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
    use crate::runs::testing::test_node;
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

    /// The point of [B14]: before this, a drifted container was skipped
    /// *because* it was alive, so editing `.fghj.yaml` and pressing "Run
    /// flow" did nothing at all.
    #[test]
    fn a_running_node_whose_config_changed_is_recreated() {
        assert!(!top_up_may_skip(true, Some(&running("abc")), Some("def")));
    }

    /// Re-resolving the spec failed, so nothing has established that the
    /// running container is wrong. Bouncing a healthy container on a guess is
    /// the more expensive of the two mistakes.
    #[test]
    fn an_inconclusive_hash_leaves_a_running_node_alone() {
        assert!(top_up_may_skip(true, Some(&running("abc")), None));
    }

    /// fghj has a live container it has no `ContainerInfo` for — its routes
    /// are not registered, so it has to be recreated to be re-described,
    /// whatever its config says.
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

    /// The default. A top-up re-runs it every time, which is the contract
    /// `run: on_start` states — the command is expected to be idempotent.
    #[test]
    fn an_on_start_task_runs_again_even_after_a_clean_exit() {
        assert!(!task_already_done(
            &task("migrate.api", "on_start"),
            Some(&finished(Some(0)))
        ));
    }

    /// `once` means "succeed once", not "be attempted once" — a migration
    /// that exited 3 has not done its job, and skipping it would leave the
    /// service behind it permanently blocked with no way to retry.
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

    /// The guard must never fire for a service: a stopped service that
    /// happens to carry an exit code is exactly what a top-up is for.
    #[test]
    fn a_service_is_never_skipped_however_it_exited() {
        let mut service = test_node("api.shop", "api", "service");
        service.run_policy = Some("once".to_string());
        assert!(!task_already_done(&service, Some(&finished(Some(0)))));
    }
}
