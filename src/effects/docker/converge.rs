//! Drives Docker start/stop/delete off `ContainerInfo::pending_action`, and
//! creates off `EnvState::pending_create`, through `Orchestrator`, and reports
//! each result back as an action. Changes nobody asked for are picked up by
//! `observe`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::task::AbortHandle;

use crate::action::Action;
use crate::activity::{ActivityGuard, ActivityHandle, ActivityKind, ActivityStatus};
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

/// What `converge` does with a snapshot: entries to spawn, and nodes whose
/// task is no longer wanted. `converge` gets the whole snapshot each time, so
/// without this a still-pending node would get a second call.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    spawn: Vec<PendingEntry>,
    /// A start called off by a Stop or Delete, which `spawn` then carries.
    abort: Vec<String>,
}

fn plan(in_flight: &HashMap<String, PendingAction>, snapshot: &[PendingEntry]) -> Plan {
    let mut plan = Plan::default();
    for entry in snapshot {
        match in_flight.get(&entry.node_id) {
            Some(action) if *action == entry.action => {}
            Some(_) => {
                plan.abort.push(entry.node_id.clone());
                plan.spawn.push(entry.clone());
            }
            None => plan.spawn.push(entry.clone()),
        }
    }
    // Gone without settling: a start with no container yet, called off.
    for (node_id, action) in in_flight {
        if *action == PendingAction::Starting && !snapshot.iter().any(|e| e.node_id == *node_id) {
            plan.abort.push(node_id.clone());
        }
    }
    plan
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
    progress: &ProgressSink,
) -> anyhow::Result<Option<ContainerInfo>> {
    let container = run.containers.get(&entry.node_id);
    match entry.action {
        PendingAction::Starting => {
            let graph = resolve(old).await?;
            graph.refuse_if_blocked()?;
            // The reducer already wrote the debug switch; `restart_node` reads it.
            let restarted = old
                .orchestrator
                .restart_node(&graph, &entry.node_id, run, Some(progress))
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
        PendingAction::Restarting => {
            anyhow::bail!("a restart is part of a run, never asked for on its own")
        }
    }
}

fn activity_kind(action: PendingAction) -> ActivityKind {
    match action {
        PendingAction::Starting | PendingAction::Restarting => ActivityKind::Start,
        PendingAction::Stopping => ActivityKind::Stop,
        PendingAction::Removing => ActivityKind::Delete,
    }
}

/// Opens the activity a create or container action reports into. One just
/// like it still running is stale: `converge` never runs two at once, so it
/// belongs to an aborted task whose guard hasn't dropped yet.
fn open_activity(
    old: &server::WorkspaceState,
    kind: ActivityKind,
    key: &str,
    opening: &str,
) -> ActivityGuard {
    let job = old.activities.start_superseding(Some(&old.id), kind, key);
    job.append(&format!("{opening}\n"));
    // Aborted with its task when a Stop calls a start off.
    job.settle_on_drop("called off")
}

/// Each node a run plans or touches becomes a child of `job`, settled by the
/// node's own report. The node `job` itself is for gets lines, not a child.
struct NodeActivities {
    job: ActivityHandle,
    own: Option<String>,
    open: HashMap<String, ActivityHandle>,
}

impl NodeActivities {
    /// Lists the plan as queued steps before any of it runs.
    fn planned(&mut self, start: &[String], stop: &[String]) {
        let steps = start
            .iter()
            .map(|id| (id, ActivityKind::Start))
            .chain(stop.iter().map(|id| (id, ActivityKind::Stop)));
        for (node_id, kind) in steps {
            if self.own.as_deref() == Some(node_id.as_str()) || self.open.contains_key(node_id) {
                continue;
            }
            let child = self.job.queued_child_of_kind(Some(kind), node_id);
            self.open.insert(node_id.clone(), child);
        }
    }

    fn working(&mut self, node_id: &str, action: Option<PendingAction>) {
        let job = &self.job;
        let own = self.own.as_deref() == Some(node_id);
        match action {
            Some(action) => {
                let line = format!("{action} {node_id}\n");
                if own {
                    job.append(&line);
                    return;
                }
                let child = self
                    .open
                    .entry(node_id.to_string())
                    .or_insert_with(|| job.child_of_kind(Some(activity_kind(action)), node_id));
                child.begin();
                child.append(&line);
            }
            None => {
                // The node's events pane has the error itself.
                let line = format!("{node_id} failed\n");
                match self.open.remove(node_id) {
                    Some(child) => {
                        child.append(&line);
                        child.settle(&Err(anyhow::anyhow!("see {node_id}'s events")));
                    }
                    None => job.append(&line),
                }
            }
        }
    }

    fn done(&mut self, info: &ContainerInfo) {
        let job = &self.job;
        let line = format!("{} {}\n", info.node_id, info.observed.status);
        match self.open.remove(&info.node_id) {
            Some(child) => {
                child.append(&line);
                child.settle(&Ok(()));
            }
            None => job.append(&line),
        }
    }

    /// Settles the planned steps the run never reported on: nodes it found
    /// current and left alone. After a failure, one that isn't up was blocked
    /// and is left for the job's settle to cancel.
    fn finish(self, env: Option<&EnvState>, succeeded: bool) {
        for (node_id, child) in self.open {
            if child.status() != Some(ActivityStatus::Queued) {
                continue;
            }
            let observed = env
                .and_then(|env| env.containers.get(&node_id))
                .map(|c| &c.observed);
            let line = match observed {
                Some(o) if o.status == "running" => "already up to date",
                Some(o) if o.exit_code == Some(0) => "already done",
                _ if succeeded => "nothing to do",
                _ => continue,
            };
            child.append(&format!("{node_id}: {line}\n"));
            child.settle(&Ok(()));
        }
    }
}

/// Turns a run's progress into actions, so the UI follows each node as it goes,
/// and into `nodes`. `skip` is the node a container action is for: its own
/// settle reports it, and clearing its mark early would make the reducer drop
/// that settle.
fn forward_progress(
    actor: ActorHandle,
    skip: Option<String>,
    mut nodes: NodeActivities,
) -> (ProgressSink, tokio::task::JoinHandle<NodeActivities>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RunProgress>();
    let drain = tokio::spawn(async move {
        while let Some(p) = rx.recv().await {
            let action = match p {
                RunProgress::Planned { start, stop } => {
                    nodes.planned(&start, &stop);
                    continue;
                }
                RunProgress::Working { node_id, action } => {
                    if skip.as_ref() == Some(&node_id) {
                        continue;
                    }
                    nodes.working(&node_id, action);
                    Action::EnvCreateWorking { node_id, action }
                }
                RunProgress::Done(done) => {
                    let NodeDone {
                        network,
                        sidecar_container_name,
                        sidecar_ip,
                        info,
                    } = *done;
                    if skip.as_ref() == Some(&info.node_id) {
                        continue;
                    }
                    nodes.done(&info);
                    Action::EnvCreateProgress {
                        network,
                        sidecar_container_name,
                        sidecar_ip,
                        info,
                    }
                }
            };
            let _ = actor.dispatch(action).await;
        }
        nodes
    });
    (tx, drain)
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
    of: PendingAction,
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
        let of = Some(self.of);
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
                    of,
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
    in_flight: HashMap<String, (PendingAction, AbortHandle)>,
    create_in_flight: bool,
}

impl DockerConvergeEffect {
    pub fn new(old: Arc<server::WorkspaceState>, actor: ActorHandle) -> Self {
        Self {
            old,
            actor,
            in_flight: HashMap::new(),
            create_in_flight: false,
        }
    }

    /// Aborting drops the start's `SettleGuard`, whose report the reducer
    /// ignores: the node has moved on to the action that called it off.
    fn abort_start(&mut self, node_id: &str) {
        let Some((_, task)) = self.in_flight.remove(node_id) else {
            return;
        };
        // A start that settled on its own leaves the snapshot the same way.
        if task.is_finished() {
            return;
        }
        task.abort();
        daemon_log::info(format!("fghjd: called off starting {node_id}"));
        let old = self.old.clone();
        let node_id = node_id.to_string();
        supervisor::supervise("start called off", async move {
            old.orchestrator.record_start_called_off(&node_id).await;
        });
    }

    fn spawn_action(&self, entry: PendingEntry) -> AbortHandle {
        let old = self.old.clone();
        let guard = SettleGuard {
            actor: self.actor.clone(),
            node_id: entry.node_id.clone(),
            of: entry.action,
            outcome: None,
        };
        let run = self.actor.current().env.clone();
        let actor = self.actor.clone();
        let job = open_activity(
            &old,
            activity_kind(entry.action),
            &entry.node_id,
            &format!("{} {}", entry.action, entry.node_id),
        );
        supervisor::supervise("container action", async move {
            let mut cascaded = Vec::new();
            let nodes = NodeActivities {
                job: (*job).clone(),
                own: Some(entry.node_id.clone()),
                open: HashMap::new(),
            };
            let (tx, drain) = forward_progress(actor.clone(), Some(entry.node_id.clone()), nodes);
            let result = match run {
                Some(run) => perform(&old, &entry, &run, &mut cascaded, &tx)
                    .await
                    .map_err(|e| format!("{e:#}")),
                None => Err("no environment yet".to_string()),
            };
            // Drained first, so a late progress report can't land after the settle.
            drop(tx);
            match drain.await {
                Ok(nodes) => nodes.finish(actor.current().env.as_ref(), result.is_ok()),
                Err(e) => {
                    daemon_log::warn(format!("container-action progress drain task failed: {e}"))
                }
            }
            for info in &cascaded {
                job.append(&format!("{} {}\n", info.node_id, info.observed.status));
            }
            job.settle(
                &result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|e| anyhow::anyhow!("{e}")),
            );
            for info in cascaded {
                let _ = actor
                    .dispatch(Action::ContainerActionSettled {
                        node_id: info.node_id.clone(),
                        of: None,
                        result: Ok(Some(info)),
                    })
                    .await;
            }
            guard.settle(result);
        })
    }

    fn spawn_create(&self, plan: EnvSpec) {
        let old = self.old.clone();
        let guard = CreateSettleGuard {
            actor: self.actor.clone(),
            outcome: None,
        };
        let actor = self.actor.clone();
        let prior = self.actor.current().env.clone();
        let (kind, key, own) = match &plan {
            EnvSpec::Flow(flow) => (ActivityKind::Switch, flow, None),
            EnvSpec::Node(node) => (ActivityKind::Start, node, Some(node.clone())),
        };
        let opening = match kind {
            ActivityKind::Switch => format!("switching to {key}"),
            _ => format!("starting {key}"),
        };
        let job = open_activity(&old, kind, key, &opening);
        supervisor::supervise("env create", async move {
            let nodes = NodeActivities {
                job: (*job).clone(),
                own,
                open: HashMap::new(),
            };
            // Translated here: `orchestrator/` knows nothing about actors.
            let (tx, drain) = forward_progress(actor.clone(), None, nodes);
            let result = perform_create(&old, &plan, prior.as_ref(), Some(&tx)).await;
            // Closing the channel lets the drain finish first, so a late progress report
            // can't land after `EnvCreateSettled`.
            drop(tx);
            match drain.await {
                Ok(nodes) => nodes.finish(actor.current().env.as_ref(), result.is_ok()),
                Err(e) => daemon_log::warn(format!("env-create progress drain task failed: {e}")),
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
            job.settle(
                &result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|e| anyhow::anyhow!("{}", e.message)),
            );
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
        let actions = self
            .in_flight
            .iter()
            .map(|(node_id, (action, _))| (node_id.clone(), *action))
            .collect();
        let Plan { spawn, abort } = plan(&actions, &snapshot.pending);
        for node_id in abort {
            self.abort_start(&node_id);
        }
        // Settled ones drop out; their tasks are done or finishing.
        self.in_flight
            .retain(|node_id, _| snapshot.pending.iter().any(|e| e.node_id == *node_id));
        for entry in spawn {
            let task = self.spawn_action(entry.clone());
            self.in_flight.insert(entry.node_id, (entry.action, task));
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

    fn node_activities(own: Option<&str>) -> (Arc<crate::activity::ActivityLog>, NodeActivities) {
        let log = Arc::new(crate::activity::ActivityLog::default());
        let job = log.start(Some("ws"), ActivityKind::Switch, "app").unwrap();
        let nodes = NodeActivities {
            job,
            own: own.map(str::to_string),
            open: HashMap::new(),
        };
        (log, nodes)
    }

    #[test]
    fn each_node_a_switch_touches_is_a_child_settled_by_its_report() {
        let (log, mut nodes) = node_activities(None);
        nodes.working("db", Some(PendingAction::Starting));
        nodes.working("old", Some(PendingAction::Stopping));
        nodes.working("api", Some(PendingAction::Starting));
        let mut db = testing::container("db");
        db.observed.status = "running".into();
        nodes.done(&db);
        nodes.working("api", None);

        let all = log.list(Some("ws"));
        let child = |key: &str| all.iter().find(|a| a.key == key).unwrap().clone();
        assert_eq!(child("db").kind, ActivityKind::Start);
        assert_eq!(child("db").status, ActivityStatus::Done);
        assert_eq!(child("old").kind, ActivityKind::Stop);
        assert_eq!(child("old").status, ActivityStatus::Running);
        assert_eq!(child("api").status, ActivityStatus::Error);
        let switch = child("app");
        assert!(switch.log.contains("starting db\n"), "{}", switch.log);
        assert!(switch.log.contains("db running\n"), "{}", switch.log);
        assert!(switch.log.contains("api failed"), "{}", switch.log);
    }

    #[test]
    fn the_plan_is_listed_waiting_and_what_the_run_left_alone_is_settled_by_finish() {
        let (log, mut nodes) = node_activities(None);
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        nodes.planned(&ids(&["db", "api", "web"]), &ids(&["old"]));
        let status = |key: &str| {
            log.list(Some("ws"))
                .into_iter()
                .find(|a| a.key == key)
                .unwrap()
                .status
        };
        assert_eq!(status("db"), ActivityStatus::Queued);
        assert_eq!(status("old"), ActivityStatus::Queued);

        nodes.working("old", Some(PendingAction::Stopping));
        assert_eq!(status("old"), ActivityStatus::Running);
        nodes.working("db", Some(PendingAction::Starting));
        nodes.done(&container("db", None));

        let mut api = container("api", None);
        api.observed.status = "running".into();
        let env = state_with(vec![api]).env;
        let job = nodes.job.clone();
        nodes.finish(env.as_ref(), false);
        job.settle(&Err(anyhow::anyhow!("old wouldn't stop")));

        let all = log.list(Some("ws"));
        assert_eq!(all.len(), 5, "one child per node, no duplicates");
        assert_eq!(status("db"), ActivityStatus::Done);
        assert_eq!(status("api"), ActivityStatus::Done);
        // Never reached and not up after a failure: blocked.
        assert_eq!(status("web"), ActivityStatus::Cancelled);
        assert_eq!(status("old"), ActivityStatus::Cancelled);
        assert_eq!(
            all.iter().find(|a| a.key == "old").unwrap().kind,
            ActivityKind::Stop
        );
    }

    #[test]
    fn the_node_a_job_is_for_is_not_a_planned_step() {
        let (log, mut nodes) = node_activities(Some("web"));
        nodes.planned(&["db".to_string(), "web".to_string()], &[]);
        let keys: Vec<String> = log.list(Some("ws")).into_iter().map(|a| a.key).collect();
        assert_eq!(keys, ["db", "app"]);
    }

    #[test]
    fn the_node_a_job_is_for_gets_lines_not_a_child() {
        let (log, mut nodes) = node_activities(Some("web"));
        nodes.working("web", Some(PendingAction::Starting));
        nodes.done(&testing::container("web"));
        assert_eq!(log.list(Some("ws")).len(), 1);
        assert!(log.list(Some("ws"))[0].log.contains("starting web"));
    }

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

    fn in_flight(entries: &[(&str, PendingAction)]) -> HashMap<String, PendingAction> {
        entries
            .iter()
            .map(|(node_id, action)| (node_id.to_string(), *action))
            .collect()
    }

    #[test]
    fn plan_spawns_every_entry_when_nothing_is_already_in_flight() {
        let snapshot = vec![entry("web", PendingAction::Starting)];
        let plan = plan(&HashMap::new(), &snapshot);
        assert_eq!(plan.spawn, snapshot);
        assert!(plan.abort.is_empty());
    }

    #[test]
    fn plan_does_not_respawn_an_entry_already_in_flight() {
        let in_flight = in_flight(&[("web", PendingAction::Starting)]);
        let snapshot = vec![entry("web", PendingAction::Starting)];
        assert_eq!(plan(&in_flight, &snapshot), Plan::default());
    }

    #[test]
    fn plan_has_nothing_to_do_for_a_stop_that_settled() {
        let in_flight = in_flight(&[("web", PendingAction::Stopping)]);
        assert_eq!(plan(&in_flight, &[]), Plan::default());
    }

    #[test]
    fn plan_only_spawns_the_newly_pending_entry_among_several() {
        let in_flight = in_flight(&[("web", PendingAction::Starting)]);
        let snapshot = vec![
            entry("web", PendingAction::Starting),
            entry("api", PendingAction::Stopping),
        ];
        let plan = plan(&in_flight, &snapshot);
        assert_eq!(plan.spawn, vec![entry("api", PendingAction::Stopping)]);
        assert!(plan.abort.is_empty());
    }

    #[test]
    fn plan_calls_off_a_start_a_stop_took_over() {
        let in_flight = in_flight(&[("web", PendingAction::Starting)]);
        let snapshot = vec![entry("web", PendingAction::Stopping)];
        let plan = plan(&in_flight, &snapshot);
        assert_eq!(plan.abort, vec!["web".to_string()]);
        assert_eq!(plan.spawn, snapshot);
    }

    #[test]
    fn plan_calls_off_a_start_whose_node_went_away() {
        let in_flight = in_flight(&[("web", PendingAction::Starting)]);
        let plan = plan(&in_flight, &[]);
        assert_eq!(plan.abort, vec!["web".to_string()]);
        assert!(plan.spawn.is_empty());
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
            of: PendingAction::Starting,
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
                of: PendingAction::Stopping,
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
