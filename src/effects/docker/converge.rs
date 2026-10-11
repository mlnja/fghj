//! Drives Docker start/stop/delete off `ContainerInfo::pending_action`, and
//! creates off `EnvState::pending_create`, through `Orchestrator`, and reports
//! each result back as an action. Changes nobody asked for are picked up by
//! `observe`.

use std::collections::HashSet;
use std::sync::Arc;

use crate::action::Action;
use crate::actor::ActorHandle;
use crate::daemon_log;
use crate::effects::Effect;
use crate::orchestrator::order::dependents_of;
use crate::orchestrator::progress::{NodeDone, ProgressSink, RunProgress};
use crate::resolver;
use crate::server;
use crate::state::{
    ContainerInfo, EnvCreateError, EnvSpec, EnvState, PendingAction, WorkspaceState,
};
use crate::supervisor;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEntry {
    node_id: String,
    action: PendingAction,
}

fn extract_pending(state: &WorkspaceState) -> Vec<PendingEntry> {
    state
        .env
        .iter()
        // While a create is in flight, its containers' marks are the create's
        // progress, not jobs; acting on them started each node twice.
        .filter(|env| env.pending_create.is_none())
        .flat_map(|env| {
            env.containers.values().filter_map(|c| {
                c.pending_action.map(|action| PendingEntry {
                    node_id: c.node_id.clone(),
                    action,
                })
            })
        })
        .collect()
}

fn extract_pending_create(state: &WorkspaceState) -> Option<EnvSpec> {
    state
        .env
        .as_ref()
        .and_then(|env| env.pending_create.clone())
}

/// Spawns entries not already in flight. `converge` gets the whole snapshot
/// each time, so without this a still-pending node would get a second call.
fn plan(
    in_flight: &HashSet<String>,
    snapshot: &[PendingEntry],
) -> (Vec<PendingEntry>, HashSet<String>) {
    let mut still_in_flight = HashSet::with_capacity(snapshot.len());
    let mut to_spawn = Vec::new();
    for entry in snapshot {
        if !in_flight.contains(&entry.node_id) {
            to_spawn.push(entry.clone());
        }
        still_in_flight.insert(entry.node_id.clone());
    }
    (to_spawn, still_in_flight)
}

/// At most one create at a time: two would fight over the same containers.
fn plan_create(in_flight: bool, snapshot: Option<&EnvSpec>) -> (Option<EnvSpec>, bool) {
    match snapshot {
        Some(plan) if !in_flight => (Some(plan.clone()), true),
        Some(_) => (None, true),
        None => (None, false),
    }
}

/// Performs `entry`'s action. Starting resolves a fresh graph so a recent
/// `.fghj.yaml` edit is seen. Other containers touched (requirements,
/// dependents) go into `cascaded` so their state is reported too.
async fn perform(
    old: &server::WorkspaceState,
    entry: &PendingEntry,
    run: &EnvState,
    cascaded: &mut Vec<ContainerInfo>,
) -> anyhow::Result<Option<ContainerInfo>> {
    let container = run.containers.get(&entry.node_id);
    match entry.action {
        PendingAction::Starting => {
            let graph = resolve(old).await?;
            graph.refuse_if_blocked()?;
            // The reducer already wrote the debug switch; `restart_node` reads it.
            let restarted = old
                .orchestrator
                .restart_node(&graph, &entry.node_id, run)
                .await?;
            cascaded.extend(restarted.others);
            restarted.node.map(Some).map_err(anyhow::Error::msg)
        }
        PendingAction::Stopping => {
            let container =
                container.ok_or_else(|| anyhow::anyhow!("no such node: {}", entry.node_id))?;
            // Dependents stop first. Best effort: an unresolvable graph means this node
            // stops alone.
            let dependents = match resolve(old).await {
                Ok(graph) => dependents_of(&entry.node_id, &graph.edges),
                Err(_) => Vec::new(),
            };
            for dependent in dependents.iter().rev() {
                let Some(c) = run.containers.get(dependent) else {
                    continue;
                };
                if c.pending_action.is_some() || c.observed.status != "running" {
                    continue;
                }
                match old.orchestrator.stop_container(dependent, c).await {
                    Ok(info) => cascaded.push(info),
                    Err(e) => daemon_log::warn(format!(
                        "fghjd: stopping {dependent} before {} failed: {e:#}",
                        entry.node_id
                    )),
                }
            }
            let info = old
                .orchestrator
                .stop_container(&entry.node_id, container)
                .await?;
            Ok(Some(info))
        }
        PendingAction::Removing => {
            let container =
                container.ok_or_else(|| anyhow::anyhow!("no such node: {}", entry.node_id))?;
            old.orchestrator
                .remove_container(&entry.node_id, container)
                .await?;
            Ok(None)
        }
    }
}

async fn resolve(old: &server::WorkspaceState) -> anyhow::Result<resolver::Graph> {
    resolver::resolve_universe_async(old.path.clone()).await
}

async fn perform_create(
    old: &server::WorkspaceState,
    plan: &EnvSpec,
    prior: Option<&EnvState>,
    progress: Option<&ProgressSink>,
) -> Result<EnvState, EnvCreateError> {
    let graph = resolve(old).await?;
    graph.refuse_if_blocked()?;
    old.orchestrator
        .ensure_running(&graph, plan, prior, progress)
        .await
}

/// Reports the outcome on drop if `settle` wasn't called, so a panic or early
/// return never leaves a node busy forever.
struct SettleGuard {
    actor: ActorHandle,
    node_id: String,
    outcome: Option<Result<Option<ContainerInfo>, String>>,
}

impl SettleGuard {
    fn settle(mut self, outcome: Result<Option<ContainerInfo>, String>) {
        self.outcome = Some(outcome);
    }
}

impl Drop for SettleGuard {
    fn drop(&mut self) {
        let actor = self.actor.clone();
        let node_id = std::mem::take(&mut self.node_id);
        let outcome = self.outcome.take().unwrap_or_else(|| {
            Err(
                "action task ended without reporting a result (likely a panic or cancellation)"
                    .to_string(),
            )
        });
        supervisor::supervise("container action settled dispatch", async move {
            let _ = actor
                .dispatch(Action::ContainerActionSettled {
                    node_id,
                    result: outcome,
                })
                .await;
        });
    }
}

struct CreateSettleGuard {
    actor: ActorHandle,
    outcome: Option<Result<EnvState, EnvCreateError>>,
}

impl CreateSettleGuard {
    fn settle(mut self, outcome: Result<EnvState, EnvCreateError>) {
        self.outcome = Some(outcome);
    }
}

impl Drop for CreateSettleGuard {
    fn drop(&mut self) {
        let actor = self.actor.clone();
        let outcome = self.outcome.take().unwrap_or_else(|| {
            Err(EnvCreateError::bare(
                "environment creation task ended without reporting a result (likely a panic or cancellation)",
            ))
        });
        supervisor::supervise("env create settled dispatch", async move {
            let _ = actor
                .dispatch(Action::EnvCreateSettled { result: outcome })
                .await;
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvergeSnapshot {
    pending: Vec<PendingEntry>,
    create: Option<EnvSpec>,
}

pub struct DockerConvergeEffect {
    old: Arc<server::WorkspaceState>,
    actor: ActorHandle,
    in_flight: HashSet<String>,
    create_in_flight: bool,
}

impl DockerConvergeEffect {
    pub fn new(old: Arc<server::WorkspaceState>, actor: ActorHandle) -> Self {
        Self {
            old,
            actor,
            in_flight: HashSet::new(),
            create_in_flight: false,
        }
    }

    fn spawn_action(&self, entry: PendingEntry) {
        let old = self.old.clone();
        let guard = SettleGuard {
            actor: self.actor.clone(),
            node_id: entry.node_id.clone(),
            outcome: None,
        };
        let run = self.actor.current().env.clone();
        let actor = self.actor.clone();
        supervisor::supervise("container action", async move {
            let mut cascaded = Vec::new();
            let result = match run {
                Some(run) => perform(&old, &entry, &run, &mut cascaded)
                    .await
                    .map_err(|e| format!("{e:#}")),
                None => Err("no environment yet".to_string()),
            };
            for info in cascaded {
                let _ = actor
                    .dispatch(Action::ContainerActionSettled {
                        node_id: info.node_id.clone(),
                        result: Ok(Some(info)),
                    })
                    .await;
            }
            guard.settle(result);
        });
    }

    fn spawn_create(&self, plan: EnvSpec) {
        let old = self.old.clone();
        let guard = CreateSettleGuard {
            actor: self.actor.clone(),
            outcome: None,
        };
        let actor = self.actor.clone();
        let prior = self.actor.current().env.clone();
        supervisor::supervise("env create", async move {
            // Translated here: `orchestrator/` knows nothing about actors.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RunProgress>();
            let progress_actor = actor.clone();
            let drain = tokio::spawn(async move {
                while let Some(p) = rx.recv().await {
                    let action = match p {
                        RunProgress::Working { node_id, action } => {
                            Action::EnvCreateWorking { node_id, action }
                        }
                        RunProgress::Done(done) => {
                            let NodeDone {
                                network,
                                sidecar_container_name,
                                sidecar_ip,
                                info,
                            } = *done;
                            Action::EnvCreateProgress {
                                network,
                                sidecar_container_name,
                                sidecar_ip,
                                info,
                            }
                        }
                    };
                    let _ = progress_actor.dispatch(action).await;
                }
            });
            let result = perform_create(&old, &plan, prior.as_ref(), Some(&tx)).await;
            // Closing the channel lets the drain finish first, so a late progress report
            // can't land after `EnvCreateSettled`.
            drop(tx);
            if let Err(e) = drain.await {
                daemon_log::warn(format!("env-create progress drain task failed: {e}"));
            }
            // The HTTP caller got its 200 long ago; this is the only trace of a failure.
            if let Err(e) = &result {
                let came_up = e.partial.as_ref().map(|p| p.containers.len()).unwrap_or(0);
                daemon_log::warn(format!(
                    "fghjd: the environment failed to come up fully ({came_up} \
                     container(s) running): {}",
                    e.message
                ));
            }
            guard.settle(result);
        });
    }
}

impl Effect for DockerConvergeEffect {
    type Snapshot = ConvergeSnapshot;

    fn extract(&self, state: &WorkspaceState) -> Self::Snapshot {
        ConvergeSnapshot {
            pending: extract_pending(state),
            create: extract_pending_create(state),
        }
    }

    fn converge(&mut self, snapshot: &Self::Snapshot) -> anyhow::Result<()> {
        let (to_spawn, still_in_flight) = plan(&self.in_flight, &snapshot.pending);
        self.in_flight = still_in_flight;
        for entry in to_spawn {
            self.spawn_action(entry);
        }

        let (to_create, still_creating) =
            plan_create(self.create_in_flight, snapshot.create.as_ref());
        self.create_in_flight = still_creating;
        if let Some(plan) = to_create {
            self.spawn_create(plan);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testing::{self, workspace as state_with};
    use std::time::Duration;

    fn container(node_id: &str, pending: Option<PendingAction>) -> ContainerInfo {
        let mut c = testing::container(node_id);
        c.pending_action = pending;
        c
    }

    fn with_pending_create(containers: Vec<ContainerInfo>, plan: EnvSpec) -> WorkspaceState {
        let mut state = state_with(containers);
        state.env.as_mut().unwrap().pending_create = Some(plan);
        state
    }

    fn entry(node_id: &str, action: PendingAction) -> PendingEntry {
        PendingEntry {
            node_id: node_id.into(),
            action,
        }
    }

    #[test]
    fn extract_pending_only_includes_containers_with_a_pending_action() {
        let state = state_with(vec![
            container("web", Some(PendingAction::Starting)),
            container("api", None),
        ]);
        let snapshot = extract_pending(&state);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].node_id, "web");
    }

    #[test]
    fn extract_pending_ignores_the_marks_of_an_in_flight_create() {
        let state = with_pending_create(
            vec![container("web", Some(PendingAction::Starting))],
            EnvSpec::Flow("app/main".into()),
        );
        assert!(extract_pending(&state).is_empty());
    }

    #[test]
    fn extract_pending_of_an_idle_workspace_is_empty() {
        let state = state_with(vec![container("web", None)]);
        assert!(extract_pending(&state).is_empty());
    }

    #[test]
    fn extract_pending_create_reads_the_environments_intent() {
        let plan = EnvSpec::Flow("app/main".into());
        let state = with_pending_create(vec![], plan.clone());
        assert_eq!(extract_pending_create(&state), Some(plan));
    }

    #[test]
    fn extract_pending_create_of_an_idle_workspace_is_none() {
        assert_eq!(extract_pending_create(&state_with(vec![])), None);
        assert_eq!(extract_pending_create(&WorkspaceState::default()), None);
    }

    #[test]
    fn plan_spawns_every_entry_when_nothing_is_already_in_flight() {
        let snapshot = vec![entry("web", PendingAction::Starting)];
        let (to_spawn, still_in_flight) = plan(&HashSet::new(), &snapshot);
        assert_eq!(to_spawn.len(), 1);
        assert!(still_in_flight.contains("web"));
    }

    #[test]
    fn plan_does_not_respawn_an_entry_already_in_flight() {
        let in_flight = HashSet::from(["web".to_string()]);
        let snapshot = vec![entry("web", PendingAction::Starting)];
        let (to_spawn, still_in_flight) = plan(&in_flight, &snapshot);
        assert!(to_spawn.is_empty());
        assert!(still_in_flight.contains("web"));
    }

    #[test]
    fn plan_drops_bookkeeping_for_an_entry_that_settled() {
        let in_flight = HashSet::from(["web".to_string()]);
        let (to_spawn, still_in_flight) = plan(&in_flight, &[]);
        assert!(to_spawn.is_empty());
        assert!(still_in_flight.is_empty());
    }

    #[test]
    fn plan_only_spawns_the_newly_pending_entry_among_several() {
        let in_flight = HashSet::from(["web".to_string()]);
        let snapshot = vec![
            entry("web", PendingAction::Starting),
            entry("api", PendingAction::Stopping),
        ];
        let (to_spawn, still_in_flight) = plan(&in_flight, &snapshot);
        assert_eq!(to_spawn.len(), 1);
        assert_eq!(to_spawn[0].node_id, "api");
        assert_eq!(still_in_flight.len(), 2);
    }

    #[test]
    fn plan_create_spawns_a_create_when_none_is_in_flight() {
        let spec = EnvSpec::Flow("app/main".into());
        let (to_spawn, in_flight) = plan_create(false, Some(&spec));
        assert_eq!(to_spawn, Some(spec));
        assert!(in_flight);
    }

    #[test]
    fn plan_create_does_not_respawn_a_create_already_in_flight() {
        let spec = EnvSpec::Flow("app/main".into());
        let (to_spawn, in_flight) = plan_create(true, Some(&spec));
        assert!(to_spawn.is_none());
        assert!(in_flight);
    }

    #[test]
    fn plan_create_drops_bookkeeping_for_a_create_that_settled() {
        let (to_spawn, in_flight) = plan_create(true, None);
        assert!(to_spawn.is_none());
        assert!(!in_flight);
    }

    async fn wait_until(mut check: impl FnMut() -> bool) {
        for _ in 0..200 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition never became true");
    }

    fn actor_with_pending(node_id: &str, pending: PendingAction) -> ActorHandle {
        crate::actor::spawn(state_with(vec![container(node_id, Some(pending))]))
    }

    fn web_is_settled(handle: &ActorHandle) -> bool {
        handle.current().env.as_ref().unwrap().containers["web"]
            .pending_action
            .is_none()
    }

    #[tokio::test]
    async fn settle_guard_dispatches_the_recorded_outcome_when_settled_normally() {
        let handle = actor_with_pending("web", PendingAction::Starting);
        let guard = SettleGuard {
            actor: handle.clone(),
            node_id: "web".into(),
            outcome: None,
        };
        guard.settle(Ok(Some(container("web", None))));

        wait_until(|| web_is_settled(&handle)).await;
    }

    #[tokio::test]
    async fn settle_guard_dispatches_a_fallback_outcome_when_dropped_unsettled() {
        let handle = actor_with_pending("web", PendingAction::Stopping);
        {
            let _guard = SettleGuard {
                actor: handle.clone(),
                node_id: "web".into(),
                outcome: None,
            };
        }

        wait_until(|| web_is_settled(&handle)).await;
    }

    fn actor_with_pending_create() -> ActorHandle {
        crate::actor::spawn(with_pending_create(
            vec![],
            EnvSpec::Flow("app/main".into()),
        ))
    }

    #[tokio::test]
    async fn create_settle_guard_dispatches_the_recorded_outcome_when_settled_normally() {
        let handle = actor_with_pending_create();
        let guard = CreateSettleGuard {
            actor: handle.clone(),
            outcome: None,
        };
        guard.settle(Ok(testing::run_state(vec![container("web", None)])));

        wait_until(|| {
            handle
                .current()
                .env
                .as_ref()
                .unwrap()
                .containers
                .contains_key("web")
        })
        .await;
    }

    #[tokio::test]
    async fn create_settle_guard_dispatches_a_fallback_outcome_when_dropped_unsettled() {
        let handle = actor_with_pending_create();
        {
            let _guard = CreateSettleGuard {
                actor: handle.clone(),
                outcome: None,
            };
        }

        wait_until(|| {
            handle
                .current()
                .env
                .as_ref()
                .unwrap()
                .pending_create
                .is_none()
        })
        .await;
    }
}
