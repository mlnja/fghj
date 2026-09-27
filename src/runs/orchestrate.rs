//! Whole-run orchestration: starting a run and bringing it up to date.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;

use super::health::RunBudget;
use super::naming::{DEFAULT_RUN_ID, resolve_run_id};
use super::order::topological_start_order;
use super::progress::{ProgressSink, RunProgress, report};
use super::spec::spec_hash;
use crate::docker;
use crate::resolver::{Graph, Node};
use crate::state::{ContainerInfo, RunCreateError, RunSpec, RunState};
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
/// top-up is an explicit action (`POST /runs` with no run id — "Run flow",
/// `fghj up`), and the mental model people bring to it is `docker compose
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

impl RunRegistry {
    /// `prior` is whatever the reducer already had for this run id, if
    /// anything — passed in rather than looked up, since the reducer owns
    /// the only copy. A named run always starts clean, so an existing one
    /// is torn down first.
    pub async fn start(
        &self,
        graph: &Graph,
        spec: RunSpec,
        prior: Option<&RunState>,
        progress: Option<&ProgressSink>,
    ) -> Result<RunState> {
        let run_id = resolve_run_id(spec.run_id.as_deref());

        // starting an already-running run replaces it cleanly
        if let Some(prior) = prior {
            self.stop(&run_id, prior).await?;
        }

        let network = format!("fghj-{}-{}", sanitize_label(&graph.workspace_name), run_id);
        docker::ensure_network(&self.docker, &network, &network).await?;

        let (sidecar_container_name, sidecar_ip) = match self
            .ensure_sidecar(&graph.workspace_name, &run_id, &network)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                docker::remove_network(&self.docker, &network).await;
                return Err(e);
            }
        };

        let node_map: HashMap<&str, &Node> =
            graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let target_ids: Vec<String> = graph
            .nodes
            .iter()
            .filter(|n| n.kind != "flow")
            .filter(|n| {
                spec.flow
                    .as_deref()
                    .is_none_or(|flow| n.flows.iter().any(|f| f == flow))
            })
            .map(|n| n.id.clone())
            .collect();
        let ordered_ids = topological_start_order(&target_ids, &graph.edges);

        let mut containers = BTreeMap::new();
        // One budget for the whole run, not one per node: nodes start
        // sequentially, so a per-node limit bounded nothing an impatient
        // person cares about. See `HealthBudget`, and `RunBudget` for why
        // health waits and task waits get separate deadlines.
        let budget = RunBudget::default();
        for node_id in &ordered_ids {
            let node = node_map[node_id.as_str()];
            match self
                .start_node(graph, node, &run_id, &network, Some(&sidecar_ip), &budget)
                .await
            {
                Ok(info) => {
                    // Reported before being folded into the local map so a
                    // daemon that dies on the *next* node still leaves a
                    // record of this one.
                    report(
                        progress,
                        RunProgress {
                            run_id: run_id.clone(),
                            network: network.clone(),
                            sidecar_container_name: sidecar_container_name.clone(),
                            sidecar_ip: Some(sidecar_ip.clone()),
                            info: info.clone(),
                        },
                    );
                    containers.insert(info.node_id.clone(), info);
                }
                Err(e) => {
                    for c in containers.values() {
                        docker::stop_and_remove(&self.docker, &c.desired.container_name).await;
                    }
                    docker::stop_and_remove(&self.docker, &sidecar_container_name).await;
                    docker::remove_network(&self.docker, &network).await;
                    return Err(e);
                }
            }
        }

        let state = RunState {
            run_id: run_id.clone(),
            network,
            containers,
            sidecar_container_name,
            sidecar_ip: Some(sidecar_ip),
            ..Default::default()
        };
        Ok(state)
    }

    /// Tops up the single default environment so every node reachable from
    /// `flow` (or every node in the graph, if `flow` is `None`) is running —
    /// unlike `start`, this never touches a container that's already alive.
    /// fghj models one shared set of running containers per workspace, not a
    /// separate environment per flow, so picking a flow should never restart
    /// (or duplicate) whatever's already up.
    ///
    /// Liveness is checked directly against docker on every call rather than
    /// trusting the persisted `RunState`, since a container can be
    /// stopped/removed out-of-band between calls (see `refresh`).
    ///
    /// Deliberately does *not* roll back the way `start` does when a node
    /// fails partway: this tops up the one shared default environment, so
    /// tearing down the three containers that came up because the fourth
    /// didn't would destroy exactly the progress the per-node reporting
    /// below exists to keep. Each node is reported through `progress` as it
    /// comes up, and the error additionally carries the whole partial state
    /// (`RunCreateError::partial`), so the containers stay visible and
    /// routable instead of running unseen.
    pub async fn ensure_running(
        &self,
        graph: &Graph,
        flow: Option<&str>,
        prior: Option<&RunState>,
        progress: Option<&ProgressSink>,
    ) -> Result<RunState, RunCreateError> {
        let run_id = DEFAULT_RUN_ID.to_string();
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
        state.sidecar_container_name = sidecar_container_name;
        state.sidecar_ip = Some(sidecar_ip);

        let node_map: HashMap<&str, &Node> =
            graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let target_ids: Vec<String> = graph
            .nodes
            .iter()
            .filter(|n| n.kind != "flow")
            .filter(|n| flow.is_none_or(|flow| n.flows.iter().any(|f| f == flow)))
            .map(|n| n.id.clone())
            .collect();
        let ordered_ids = topological_start_order(&target_ids, &graph.edges);
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
            if top_up_may_skip(alive, existing, fresh_hash.as_deref()) {
                continue;
            }
            if alive {
                // Narrated, because from the outside a top-up bouncing
                // something that was running fine looks like a bug.
                self.record_event(
                    &run_id,
                    &node.id,
                    "create",
                    "recreating container: config changed since it was started",
                    "ok",
                    None,
                )
                .await;
            }
            // A stopped-but-not-removed container from a previous run would
            // otherwise collide with create_container's fixed name.
            docker::stop_and_remove(&self.docker, &container_name).await;

            let info = match self
                .start_node(
                    graph,
                    node,
                    &run_id,
                    &network,
                    state.sidecar_ip.as_deref(),
                    &budget,
                )
                .await
            {
                Ok(info) => info,
                Err(e) => {
                    return Err(RunCreateError {
                        message: format!("{e:#}"),
                        partial: Some(Box::new(state)),
                    });
                }
            };
            // Reported after every node, not just at the end, so a later
            // failure — or a daemon that dies outright — doesn't lose track
            // of containers that did start successfully.
            report(
                progress,
                RunProgress {
                    run_id: run_id.clone(),
                    network: network.clone(),
                    sidecar_container_name: state.sidecar_container_name.clone(),
                    sidecar_ip: state.sidecar_ip.clone(),
                    info: info.clone(),
                },
            );
            state.containers.insert(info.node_id.clone(), info);
        }

        Ok(state)
    }
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
