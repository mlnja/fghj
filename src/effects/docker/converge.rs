//! Drives real Docker create/start/stop/delete calls off `RunState`'s two
//! convergence fields — `ContainerInfo::pending_action` (per-node
//! start/stop/delete) and `RunState::pending_create` (whole-run
//! create/top-up) — migration phase 4 (node lifecycle) and phase 5 (run
//! creation, retiring `effects::bridge`) of the architecture plan
//! (rosy-soaring-teapot.md). Reuses `runs::RunRegistry`'s existing
//! orchestration methods wholesale (`restart_container`/`stop_container`/
//! `remove_container`/`start`/`ensure_running`, see `perform`/
//! `perform_create`) rather than reimplementing container lifecycle here;
//! this effect's only responsibility is *triggering* those calls from
//! dispatched state and reporting the real result back
//! (`Action::ContainerActionSettled`/`Action::RunCreateSettled`).
//!
//! ## No more bridge
//!
//! Through migration phase 4, `effects::bridge` polled `RunRegistry::list()`
//! roughly once a second and wholesale-replaced `WorkspaceState::runs`,
//! which is how the new system ever learned anything real had happened.
//! Phase 5 deletes that poller outright: every mutation the *new* system
//! can cause (create/top-up, start, stop, delete) now reports its own real
//! result directly, via the `*SettleGuard`s below, so nothing needs to
//! re-derive it from a fresh snapshot a moment later.
//!
//! There is also nothing to translate any more. `RunRegistry` builds the
//! same `state::RunState`/`ContainerInfo` the reducer holds, so what
//! `perform`/`perform_create` return is dispatched as-is. This module used
//! to carry a set of `mirror_*` functions converting a second, flatter
//! container shape into this one — and because that shape had no field for
//! *intent*, the conversion had to reconstruct `desired.running` from the
//! observed status, quietly making the two agree and hiding exactly the
//! drift the split exists to surface.
//!
//! A container that changes state for a reason *neither* this effect nor an
//! HTTP-dispatched request caused (Docker's own restart policy reviving a
//! crashed container, an operator running `docker stop` by hand) is picked
//! up by `effects::docker::observe`, which reports
//! `Action::ContainerObserved` off the reconciler's existing once-a-second
//! inspection.

use std::collections::HashSet;
use std::sync::Arc;

use crate::action::Action;
use crate::actor::ActorHandle;
use crate::daemon_log;
use crate::effects::Effect;
use crate::resolver;
use crate::runs::progress::{ProgressSink, RunProgress};
use crate::server;
use crate::state::{
    ContainerInfo, PendingAction, RunCreateError, RunSpec, RunState, WorkspaceState,
};

/// One container currently mid-start/stop/delete, as seen by this effect's
/// `extract` — a plain projection of `ContainerInfo::pending_action`,
/// addressed the same way every other per-container `Action` is
/// (`run_id`/`node_id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEntry {
    run_id: String,
    node_id: String,
    action: PendingAction,
}

fn extract_pending(state: &WorkspaceState) -> Vec<PendingEntry> {
    state
        .runs
        .iter()
        .flat_map(|(run_id, run)| {
            run.containers.values().filter_map(move |c| {
                c.pending_action.map(|action| PendingEntry {
                    run_id: run_id.clone(),
                    node_id: c.node_id.clone(),
                    action,
                })
            })
        })
        .collect()
}

/// One run currently mid-create/top-up, as seen by this effect's `extract`
/// — a plain projection of `RunState::pending_create`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCreate {
    run_id: String,
    plan: RunSpec,
}

fn extract_pending_creates(state: &WorkspaceState) -> Vec<PendingCreate> {
    state
        .runs
        .iter()
        .filter_map(|(run_id, run)| {
            run.pending_create.clone().map(|plan| PendingCreate {
                run_id: run_id.clone(),
                plan,
            })
        })
        .collect()
}

/// Every run whose teardown has been requested but not yet performed — a
/// plain projection of `RunState::pending_teardown`.
fn extract_pending_teardowns(state: &WorkspaceState) -> Vec<String> {
    state
        .runs
        .iter()
        .filter(|(_, run)| run.pending_teardown)
        .map(|(run_id, _)| run_id.clone())
        .collect()
}

/// `plan_creates` for teardown jobs, deduped by `run_id` for the same
/// reason: tearing one run down twice concurrently would have the second
/// call racing the first over the same network and sidecar.
fn plan_teardowns(
    in_flight: &HashSet<String>,
    snapshot: &[String],
) -> (Vec<String>, HashSet<String>) {
    let mut still_in_flight = HashSet::with_capacity(snapshot.len());
    let mut to_spawn = Vec::new();
    for run_id in snapshot {
        if !in_flight.contains(run_id) {
            to_spawn.push(run_id.clone());
        }
        still_in_flight.insert(run_id.clone());
    }
    (to_spawn, still_in_flight)
}

/// Decides which of `snapshot`'s entries need a fresh Docker call spawned —
/// any not already in `in_flight` — and returns the refreshed `in_flight`
/// set to replace it with. Pure and synchronous, so it's the one part of
/// this effect's dedup logic (the actual new risk this migration phase
/// introduces: `converge` is called with the *whole* current snapshot every
/// time anything about it changes, not a diff, so without this a container
/// still pending for an unrelated reason — e.g. a sibling container's own
/// action settling — would get a redundant second Docker call spawned
/// against it) that's unit-testable without spawning a task or touching
/// Docker at all.
fn plan(
    in_flight: &HashSet<(String, String)>,
    snapshot: &[PendingEntry],
) -> (Vec<PendingEntry>, HashSet<(String, String)>) {
    let mut still_in_flight = HashSet::with_capacity(snapshot.len());
    let mut to_spawn = Vec::new();
    for entry in snapshot {
        let key = (entry.run_id.clone(), entry.node_id.clone());
        if !in_flight.contains(&key) {
            to_spawn.push(entry.clone());
        }
        still_in_flight.insert(key);
    }
    (to_spawn, still_in_flight)
}

/// `plan`'s counterpart for run-level creation jobs, deduped by `run_id`
/// alone (not `(run_id, node_id)`): `RunRegistry::start`/`ensure_running`
/// each operate on a whole run atomically, so calling either one twice
/// concurrently for the same `run_id` would be actively destructive —
/// `start` in particular tears down and recreates an already-running run
/// out from under its own in-flight sibling call.
fn plan_creates(
    in_flight: &HashSet<String>,
    snapshot: &[PendingCreate],
) -> (Vec<PendingCreate>, HashSet<String>) {
    let mut still_in_flight = HashSet::with_capacity(snapshot.len());
    let mut to_spawn = Vec::new();
    for entry in snapshot {
        if !in_flight.contains(&entry.run_id) {
            to_spawn.push(entry.clone());
        }
        still_in_flight.insert(entry.run_id.clone());
    }
    (to_spawn, still_in_flight)
}

/// Actually performs `entry`'s action against the still-current
/// `runs::RunRegistry` — the "reuse 100% of existing Docker orchestration
/// code" half of this effect. `Starting` alone needs a freshly-resolved
/// `Graph` first (real I/O `RunRegistry::restart_container` needs but a
/// pure reducer can't do) — the same `resolver::resolve_universe` call
/// `daemon::post_run_node_start` made inline before migration phase 4,
/// moved here unchanged and still not cached, since a stale graph would
/// silently ignore a `.fghj.yaml` edit made between the request and this
/// call. Returns the container's freshly re-observed state (`None` only for
/// `Removing`, where there's nothing left to observe) rather than just
/// `()`, so the caller can report real post-action truth instead of a bare
/// success/failure.
async fn perform(
    old: &server::WorkspaceState,
    entry: &PendingEntry,
    run: &RunState,
) -> anyhow::Result<Option<ContainerInfo>> {
    let container = run.containers.get(&entry.node_id);
    match entry.action {
        PendingAction::Starting => {
            let path = old.path.clone();
            let graph = tokio::task::spawn_blocking(move || resolver::resolve_universe(&path))
                .await
                .map_err(|e| anyhow::anyhow!("resolve_universe task panicked: {e}"))??;
            // Starting one node still reads the whole graph (that is how it
            // learns what to link to), so a blocking problem anywhere in
            // the workspace is a blocking problem here too.
            graph.refuse_if_blocked()?;
            let info = old
                .runs
                .restart_container(
                    &graph,
                    &entry.run_id,
                    &entry.node_id,
                    &run.network,
                    run.sidecar_ip.as_deref(),
                )
                .await?;
            Ok(Some(info))
        }
        PendingAction::Stopping => {
            let container = container.ok_or_else(|| {
                anyhow::anyhow!("no such node in run {}: {}", entry.run_id, entry.node_id)
            })?;
            // Returns the stopped container directly, so there is no
            // read-back from a second copy of the run to disagree with.
            let info = old
                .runs
                .stop_container(&entry.run_id, &entry.node_id, container)
                .await?;
            Ok(Some(info))
        }
        PendingAction::Removing => {
            let container = container.ok_or_else(|| {
                anyhow::anyhow!("no such node in run {}: {}", entry.run_id, entry.node_id)
            })?;
            old.runs
                .remove_container(&entry.run_id, &entry.node_id, container)
                .await?;
            Ok(None)
        }
    }
}

/// `perform`'s counterpart for a whole-run create/top-up: resolves a fresh
/// graph (same reasoning as `perform`'s `Starting` case), then reuses
/// `RunRegistry::start` (a named run always starts fresh) or
/// `RunRegistry::ensure_running` (the unnamed/default-environment case only
/// tops up whatever isn't already running) exactly as `daemon::post_runs`
/// called them directly before migration phase 5 — `entry.plan.run_id`
/// (the caller's original, possibly-absent intent) is what decides which,
/// not `entry.run_id` (always concrete, since it's the map key `RunState`
/// is filed under).
async fn perform_create(
    old: &server::WorkspaceState,
    entry: &PendingCreate,
    prior: Option<&RunState>,
    progress: Option<&ProgressSink>,
) -> Result<RunState, RunCreateError> {
    let path = old.path.clone();
    let graph = tokio::task::spawn_blocking(move || resolver::resolve_universe(&path))
        .await
        .map_err(|e| anyhow::anyhow!("resolve_universe task panicked: {e}"))??;
    // Nothing has been created yet, so there is no partial state to carry:
    // the `From<anyhow::Error>` conversion's `partial: None` is correct.
    graph.refuse_if_blocked()?;
    if entry.plan.run_id.is_some() {
        // `start` rolls its own half-built run back, so there is never a
        // partial state to carry — the `From<anyhow::Error>` conversion's
        // `partial: None` is the whole truth here.
        Ok(old
            .runs
            .start(&graph, entry.plan.clone(), prior, progress)
            .await?)
    } else {
        old.runs
            .ensure_running(&graph, entry.plan.flow.as_deref(), prior, progress)
            .await
    }
}

/// Dispatches `Action::ContainerActionSettled` when dropped, unless
/// [`SettleGuard::settle`] already recorded a real outcome — the RAII
/// "always reports, even on panic or an early return" guarantee the
/// architecture plan calls for (see its "In-flight dedup" section).
/// `settle` consumes `self` but that value is still dropped normally right
/// after (nothing calls `mem::forget`), so the same `Drop` impl handles both
/// the happy path (outcome already `Some`) and a panic/early-return inside
/// `perform` (outcome still `None`, since a panic unwinding through an
/// in-progress `.await` drops every live local in the async fn's frame
/// exactly like it would in a plain synchronous function).
struct SettleGuard {
    actor: ActorHandle,
    run_id: String,
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
        let run_id = std::mem::take(&mut self.run_id);
        let node_id = std::mem::take(&mut self.node_id);
        let outcome = self.outcome.take().unwrap_or_else(|| {
            Err(
                "action task ended without reporting a result (likely a panic or cancellation)"
                    .to_string(),
            )
        });
        tokio::spawn(async move {
            let _ = actor
                .dispatch(Action::ContainerActionSettled {
                    run_id,
                    node_id,
                    result: outcome,
                })
                .await;
        });
    }
}

/// `SettleGuard`'s counterpart for a whole-run create/top-up job — see
/// `SettleGuard`'s doc for the RAII guarantee this gives.
struct CreateSettleGuard {
    actor: ActorHandle,
    run_id: String,
    outcome: Option<Result<RunState, RunCreateError>>,
}

impl CreateSettleGuard {
    fn settle(mut self, outcome: Result<RunState, RunCreateError>) {
        self.outcome = Some(outcome);
    }
}

impl Drop for CreateSettleGuard {
    fn drop(&mut self) {
        let actor = self.actor.clone();
        let run_id = std::mem::take(&mut self.run_id);
        let outcome = self.outcome.take().unwrap_or_else(|| {
            Err(RunCreateError::bare(
                "run creation task ended without reporting a result (likely a panic or cancellation)",
            ))
        });
        tokio::spawn(async move {
            let _ = actor
                .dispatch(Action::RunCreateSettled {
                    run_id,
                    result: outcome,
                })
                .await;
        });
    }
}

/// `SettleGuard` for a whole-run teardown job.
struct TeardownSettleGuard {
    actor: ActorHandle,
    run_id: String,
    outcome: Option<Result<(), String>>,
}

impl TeardownSettleGuard {
    fn settle(mut self, outcome: Result<(), String>) {
        self.outcome = Some(outcome);
    }
}

impl Drop for TeardownSettleGuard {
    fn drop(&mut self) {
        let actor = self.actor.clone();
        let run_id = std::mem::take(&mut self.run_id);
        let outcome = self.outcome.take().unwrap_or_else(|| {
            Err(
                "teardown task ended without reporting a result (likely a panic or cancellation)"
                    .to_string(),
            )
        });
        tokio::spawn(async move {
            let _ = actor
                .dispatch(Action::RunTeardownSettled {
                    run_id,
                    result: outcome,
                })
                .await;
        });
    }
}

/// The combined snapshot this effect converges on: both per-node
/// start/stop/delete jobs and whole-run create/top-up jobs, bundled
/// together so a single `Effect` (wired once per workspace, see
/// `daemon::WorkspaceRegistry::wire_actor`) drives both — the plan's target
/// module layout files both under `effects/docker/converge.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvergeSnapshot {
    pending: Vec<PendingEntry>,
    creates: Vec<PendingCreate>,
    teardowns: Vec<String>,
}

/// Per-workspace `Effect` driving real Docker lifecycle and creation calls.
/// One instance per workspace actor (see
/// `daemon::WorkspaceRegistry::wire_actor`), holding that same workspace's
/// old-system `Arc<server::WorkspaceState>` — `RunRegistry` (inside it)
/// hasn't retired as the thing that actually talks to Docker, only as a
/// *stateful* struct HTTP handlers used to call directly (migration phase
/// 5).
pub struct DockerConvergeEffect {
    old: Arc<server::WorkspaceState>,
    actor: ActorHandle,
    in_flight: HashSet<(String, String)>,
    creates_in_flight: HashSet<String>,
    teardowns_in_flight: HashSet<String>,
}

impl DockerConvergeEffect {
    pub fn new(old: Arc<server::WorkspaceState>, actor: ActorHandle) -> Self {
        Self {
            old,
            actor,
            in_flight: HashSet::new(),
            creates_in_flight: HashSet::new(),
            teardowns_in_flight: HashSet::new(),
        }
    }

    fn spawn_action(&self, entry: PendingEntry) {
        let old = self.old.clone();
        let guard = SettleGuard {
            actor: self.actor.clone(),
            run_id: entry.run_id.clone(),
            node_id: entry.node_id.clone(),
            outcome: None,
        };
        // The run as the reducer currently has it — the only copy there is.
        // Taken at spawn time rather than read back out of `RunRegistry`,
        // which no longer keeps one.
        let run = self.actor.current().runs.get(&entry.run_id).cloned();
        tokio::spawn(async move {
            let result = match run {
                Some(run) => perform(&old, &entry, &run)
                    .await
                    .map_err(|e| format!("{e:#}")),
                None => Err(format!("no such run: {}", entry.run_id)),
            };
            guard.settle(result);
        });
    }

    fn spawn_create(&self, entry: PendingCreate) {
        let old = self.old.clone();
        let guard = CreateSettleGuard {
            actor: self.actor.clone(),
            run_id: entry.run_id.clone(),
            outcome: None,
        };
        let run_id = entry.run_id.clone();
        let actor = self.actor.clone();
        let prior = self.actor.current().runs.get(&entry.run_id).cloned();
        tokio::spawn(async move {
            // Per-node progress is translated into actions here rather than
            // in `runs/`, which deliberately knows nothing about actors.
            // The draining task ends when `perform_create` drops its sender.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RunProgress>();
            let progress_actor = actor.clone();
            let drain = tokio::spawn(async move {
                while let Some(p) = rx.recv().await {
                    let _ = progress_actor
                        .dispatch(Action::RunCreateProgress {
                            run_id: p.run_id,
                            network: p.network,
                            sidecar_container_name: p.sidecar_container_name,
                            sidecar_ip: p.sidecar_ip,
                            info: p.info,
                        })
                        .await;
                }
            });
            let result = perform_create(&old, &entry, prior.as_ref(), Some(&tx)).await;
            // Dropped so the drain task sees the channel close and finishes
            // applying whatever is still queued before the settle below
            // lands — otherwise a late progress report could arrive *after*
            // `RunCreateSettled` and re-add a container the settle dropped.
            drop(tx);
            let _ = drain.await;
            // The HTTP caller already had its `200` long before this ran, so
            // without a log line here a failed create leaves no trace outside
            // the per-node events table — including the case where part of a
            // top-up succeeded and the rest didn't, which is exactly when
            // someone will be looking for an explanation.
            if let Err(e) = &result {
                let came_up = e.partial.as_ref().map(|p| p.containers.len()).unwrap_or(0);
                daemon_log::warn(format!(
                    "fghjd: run '{run_id}' failed to come up fully ({came_up} \
                     container(s) running): {}",
                    e.message
                ));
            }
            guard.settle(result);
        });
    }

    fn spawn_teardown(&self, run_id: String) {
        let old = self.old.clone();
        let guard = TeardownSettleGuard {
            actor: self.actor.clone(),
            run_id: run_id.clone(),
            outcome: None,
        };
        // The state to tear down is read here rather than in `runs/`, which
        // no longer keeps a copy of anything. A run the reducer has already
        // forgotten has nothing left to stop, so that settles clean.
        let Some(state) = self.actor.current().runs.get(&run_id).cloned() else {
            guard.settle(Ok(()));
            return;
        };
        tokio::spawn(async move {
            let result = old
                .runs
                .stop(&run_id, &state)
                .await
                .map_err(|e| format!("{e:#}"));
            if let Err(e) = &result {
                daemon_log::warn(format!("fghjd: run '{run_id}' failed to tear down: {e}"));
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
            creates: extract_pending_creates(state),
            teardowns: extract_pending_teardowns(state),
        }
    }

    fn converge(&mut self, snapshot: &Self::Snapshot) -> anyhow::Result<()> {
        let (to_spawn, still_in_flight) = plan(&self.in_flight, &snapshot.pending);
        self.in_flight = still_in_flight;
        for entry in to_spawn {
            self.spawn_action(entry);
        }

        let (to_create, still_creating) = plan_creates(&self.creates_in_flight, &snapshot.creates);
        self.creates_in_flight = still_creating;
        for entry in to_create {
            self.spawn_create(entry);
        }

        let (to_tear_down, still_tearing_down) =
            plan_teardowns(&self.teardowns_in_flight, &snapshot.teardowns);
        self.teardowns_in_flight = still_tearing_down;
        for run_id in to_tear_down {
            self.spawn_teardown(run_id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ContainerDesired, ContainerObserved};
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn container(node_id: &str, pending: Option<PendingAction>) -> ContainerInfo {
        ContainerInfo {
            node_id: node_id.into(),
            desired: ContainerDesired {
                running: true,
                container_name: format!("fghj-{node_id}-1"),
                domain: format!("{node_id}.fghj.internal"),
                raw_domain: format!("{node_id}.fghj.raw.internal"),
                routes: vec![],
                additional_hosts: vec![],
                status_port: None,
                config_hash: "hash".into(),
            },
            observed: ContainerObserved::default(),
            pending_action: pending,
        }
    }

    fn run_state(containers: Vec<ContainerInfo>, pending_create: Option<RunSpec>) -> RunState {
        RunState {
            run_id: "default".into(),
            network: "fghj-net".into(),
            containers: containers
                .into_iter()
                .map(|c| (c.node_id.clone(), c))
                .collect(),
            volumes: BTreeMap::new(),
            sidecar_container_name: "fghj-sidecar".into(),
            sidecar_ip: None,
            pending_create,
            pending_teardown: false,
        }
    }

    fn state_with(containers: Vec<ContainerInfo>) -> WorkspaceState {
        let mut state = WorkspaceState::default();
        state
            .runs
            .insert("default".into(), run_state(containers, None));
        state
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
    fn extract_pending_of_an_idle_workspace_is_empty() {
        let state = state_with(vec![container("web", None)]);
        assert!(extract_pending(&state).is_empty());
    }

    #[test]
    fn extract_pending_creates_only_includes_runs_with_a_pending_create() {
        let mut state = WorkspaceState::default();
        state.runs.insert(
            "default".into(),
            run_state(
                vec![],
                Some(RunSpec {
                    run_id: None,
                    flow: None,
                }),
            ),
        );
        state.runs.insert("other".into(), run_state(vec![], None));
        let snapshot = extract_pending_creates(&state);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].run_id, "default");
    }

    #[test]
    fn extract_pending_creates_of_an_idle_workspace_is_empty() {
        let state = state_with(vec![container("web", None)]);
        assert!(extract_pending_creates(&state).is_empty());
    }

    #[test]
    fn plan_spawns_every_entry_when_nothing_is_already_in_flight() {
        let in_flight = HashSet::new();
        let snapshot = vec![PendingEntry {
            run_id: "default".into(),
            node_id: "web".into(),
            action: PendingAction::Starting,
        }];
        let (to_spawn, still_in_flight) = plan(&in_flight, &snapshot);
        assert_eq!(to_spawn.len(), 1);
        assert!(still_in_flight.contains(&("default".to_string(), "web".to_string())));
    }

    #[test]
    fn plan_does_not_respawn_an_entry_already_in_flight() {
        let mut in_flight = HashSet::new();
        in_flight.insert(("default".to_string(), "web".to_string()));
        let snapshot = vec![PendingEntry {
            run_id: "default".into(),
            node_id: "web".into(),
            action: PendingAction::Starting,
        }];
        let (to_spawn, still_in_flight) = plan(&in_flight, &snapshot);
        assert!(to_spawn.is_empty());
        assert!(still_in_flight.contains(&("default".to_string(), "web".to_string())));
    }

    #[test]
    fn plan_drops_bookkeeping_for_an_entry_that_settled() {
        let mut in_flight = HashSet::new();
        in_flight.insert(("default".to_string(), "web".to_string()));
        // "web" no longer appears in the snapshot - it settled and the
        // reducer cleared its `pending_action`.
        let (to_spawn, still_in_flight) = plan(&in_flight, &[]);
        assert!(to_spawn.is_empty());
        assert!(still_in_flight.is_empty());
    }

    #[test]
    fn plan_only_spawns_the_newly_pending_entry_among_several() {
        let mut in_flight = HashSet::new();
        in_flight.insert(("default".to_string(), "web".to_string()));
        let snapshot = vec![
            PendingEntry {
                run_id: "default".into(),
                node_id: "web".into(),
                action: PendingAction::Starting,
            },
            PendingEntry {
                run_id: "default".into(),
                node_id: "api".into(),
                action: PendingAction::Stopping,
            },
        ];
        let (to_spawn, still_in_flight) = plan(&in_flight, &snapshot);
        assert_eq!(to_spawn.len(), 1);
        assert_eq!(to_spawn[0].node_id, "api");
        assert_eq!(still_in_flight.len(), 2);
    }

    #[test]
    fn plan_creates_spawns_every_entry_when_nothing_is_already_in_flight() {
        let in_flight = HashSet::new();
        let snapshot = vec![PendingCreate {
            run_id: "default".into(),
            plan: RunSpec {
                run_id: None,
                flow: None,
            },
        }];
        let (to_spawn, still_in_flight) = plan_creates(&in_flight, &snapshot);
        assert_eq!(to_spawn.len(), 1);
        assert!(still_in_flight.contains("default"));
    }

    #[test]
    fn plan_creates_does_not_respawn_a_run_already_in_flight() {
        let mut in_flight = HashSet::new();
        in_flight.insert("default".to_string());
        let snapshot = vec![PendingCreate {
            run_id: "default".into(),
            plan: RunSpec {
                run_id: None,
                flow: None,
            },
        }];
        let (to_spawn, still_in_flight) = plan_creates(&in_flight, &snapshot);
        assert!(to_spawn.is_empty());
        assert!(still_in_flight.contains("default"));
    }

    #[test]
    fn plan_creates_drops_bookkeeping_for_a_run_that_settled() {
        let mut in_flight = HashSet::new();
        in_flight.insert("default".to_string());
        let (to_spawn, still_in_flight) = plan_creates(&in_flight, &[]);
        assert!(to_spawn.is_empty());
        assert!(still_in_flight.is_empty());
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

    #[tokio::test]
    async fn settle_guard_dispatches_the_recorded_outcome_when_settled_normally() {
        let handle = actor_with_pending("web", PendingAction::Starting);
        let guard = SettleGuard {
            actor: handle.clone(),
            run_id: "default".into(),
            node_id: "web".into(),
            outcome: None,
        };
        guard.settle(Ok(Some(container("web", None))));

        wait_until(|| {
            handle.current().runs["default"].containers["web"]
                .pending_action
                .is_none()
        })
        .await;
    }

    /// Simulates `perform` panicking (or the task being cancelled) before it
    /// ever calls `settle` — the guard must still dispatch a fallback
    /// outcome on drop, or a container would be stuck showing
    /// `pending_action: Some(..)` forever.
    #[tokio::test]
    async fn settle_guard_dispatches_a_fallback_outcome_when_dropped_unsettled() {
        let handle = actor_with_pending("web", PendingAction::Stopping);
        {
            let _guard = SettleGuard {
                actor: handle.clone(),
                run_id: "default".into(),
                node_id: "web".into(),
                outcome: None,
            };
            // Dropped here without calling `settle` - the failure path this
            // test exercises.
        }

        wait_until(|| {
            handle.current().runs["default"].containers["web"]
                .pending_action
                .is_none()
        })
        .await;
    }

    fn actor_with_pending_create(run_id: &str) -> ActorHandle {
        let mut state = WorkspaceState::default();
        state.runs.insert(
            run_id.into(),
            run_state(
                vec![],
                Some(RunSpec {
                    run_id: None,
                    flow: None,
                }),
            ),
        );
        crate::actor::spawn(state)
    }

    #[tokio::test]
    async fn create_settle_guard_dispatches_the_recorded_outcome_when_settled_normally() {
        let handle = actor_with_pending_create("default");
        let guard = CreateSettleGuard {
            actor: handle.clone(),
            run_id: "default".into(),
            outcome: None,
        };
        let created = run_state(vec![container("web", None)], None);
        guard.settle(Ok(created));

        wait_until(|| {
            handle.current().runs["default"]
                .containers
                .contains_key("web")
        })
        .await;
    }

    /// Same "always reports, even on panic or cancellation" guarantee as
    /// `settle_guard_dispatches_a_fallback_outcome_when_dropped_unsettled`,
    /// for the run-creation guard: a dropped-without-settling job must still
    /// clear `pending_create`, or a run would be stuck looking permanently
    /// mid-creation.
    #[tokio::test]
    async fn create_settle_guard_dispatches_a_fallback_outcome_when_dropped_unsettled() {
        let handle = actor_with_pending_create("default");
        {
            let _guard = CreateSettleGuard {
                actor: handle.clone(),
                run_id: "default".into(),
                outcome: None,
            };
        }

        wait_until(|| handle.current().runs["default"].pending_create.is_none()).await;
    }
}
