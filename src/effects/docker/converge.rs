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
//! `mirror_run`/`mirror_container`/`mirror_route` (below) are the same pure
//! translation functions `effects::bridge` used, relocated here rather than
//! deleted with the rest of it — they're still exactly what's needed to
//! turn a `runs::RunState` (the old system's real Docker-facing state) into
//! the new system's `state::RunState` shape at the one-shot moments this
//! effect actually observes fresh truth: right after `perform`/
//! `perform_create` finish, and once at `daemon::WorkspaceRegistry::wire_actor`
//! time to seed a freshly-wired actor with whatever the old system already
//! knows about (see `mirror_runs`, `pub` for exactly that caller).
//!
//! One known gap this leaves open: a container that changes state for a
//! reason *neither* this effect nor an HTTP-dispatched request caused
//! (Docker's own restart policy reviving a crashed container, an operator
//! running `docker stop` by hand, `spawn_reconciler`'s drift correction)
//! no longer has anything to notice and report it into the new system —
//! `effects::bridge` used to catch that incidentally, just by re-polling
//! everything every second. Closing that gap for real is `effects::docker::observe`,
//! the dedicated Docker-status poller the plan's target module layout
//! already reserves for a later migration phase; until it exists, the
//! `hosts`/`dns`/`raw_net` effects (which do read `ContainerInfo::observed`)
//! can go stale for a container whose state changed that way.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use crate::action::Action;
use crate::actor::ActorHandle;
use crate::effects::Effect;
use crate::resolver;
use crate::runs;
use crate::server;
use crate::state::{
    ContainerDesired, ContainerInfo, ContainerObserved, PendingAction, PortRoute, RunSpec,
    RunState, SyncStatus, WorkspaceState,
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
) -> anyhow::Result<Option<runs::ContainerInfo>> {
    match entry.action {
        PendingAction::Starting => {
            let path = old.path.clone();
            let graph = tokio::task::spawn_blocking(move || resolver::resolve_universe(&path))
                .await
                .map_err(|e| anyhow::anyhow!("resolve_universe task panicked: {e}"))??;
            let info = old
                .runs
                .restart_container(&graph, &entry.run_id, &entry.node_id)
                .await?;
            Ok(Some(info))
        }
        PendingAction::Stopping => {
            old.runs
                .stop_container(&entry.run_id, &entry.node_id)
                .await?;
            let info = old
                .runs
                .get(&entry.run_id)
                .and_then(|r| {
                    r.containers
                        .into_iter()
                        .find(|c| c.node_id == entry.node_id)
                })
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "container {} vanished from run {} immediately after stopping",
                        entry.node_id,
                        entry.run_id
                    )
                })?;
            Ok(Some(info))
        }
        PendingAction::Removing => {
            old.runs
                .remove_container(&entry.run_id, &entry.node_id)
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
) -> anyhow::Result<RunState> {
    let path = old.path.clone();
    let graph = tokio::task::spawn_blocking(move || resolver::resolve_universe(&path))
        .await
        .map_err(|e| anyhow::anyhow!("resolve_universe task panicked: {e}"))??;
    let run = if entry.plan.run_id.is_some() {
        old.runs
            .start(
                &graph,
                runs::RunSpec {
                    run_id: entry.plan.run_id.clone(),
                    flow: entry.plan.flow.clone(),
                },
            )
            .await?
    } else {
        old.runs
            .ensure_running(&graph, entry.plan.flow.as_deref())
            .await?
    };
    Ok(mirror_run(&run))
}

/// Pure conversion from the old system's whole `runs::RunState` snapshot to
/// the new system's `state::RunState` shape — used both by
/// `daemon::WorkspaceRegistry::wire_actor` (to seed a freshly-wired actor
/// with whatever the old system already has, e.g. runs reconciled from a
/// previous `fghjd` lifetime) and, indirectly via `mirror_run`, by
/// `perform_create`'s post-creation translation.
pub fn mirror_runs(old_runs: &[runs::RunState]) -> BTreeMap<String, RunState> {
    old_runs
        .iter()
        .map(|run| (run.run_id.clone(), mirror_run(run)))
        .collect()
}

fn mirror_run(run: &runs::RunState) -> RunState {
    RunState {
        run_id: run.run_id.clone(),
        network: run.network.clone(),
        containers: run
            .containers
            .iter()
            .map(|c| (c.node_id.clone(), mirror_container(c)))
            .collect(),
        // The old system never tracked volumes as state of their own (see
        // `state::VolumeInfo`'s doc) — nothing to mirror them from yet.
        volumes: BTreeMap::new(),
        sidecar_container_name: run.sidecar_container_name.clone(),
        sidecar_ip: run.sidecar_ip.clone(),
        pending_create: None,
    }
}

fn mirror_container(c: &runs::ContainerInfo) -> ContainerInfo {
    ContainerInfo {
        node_id: c.node_id.clone(),
        desired: ContainerDesired {
            running: c.status == "running",
            container_name: c.container_name.clone(),
            domain: c.domain.clone(),
            raw_domain: c.raw_domain.clone(),
            routes: c.routes.iter().map(mirror_route).collect(),
            additional_hosts: c.additional_hosts.clone(),
            status_port: c.status_port.clone(),
            config_hash: c.config_hash.clone(),
        },
        observed: ContainerObserved {
            status: c.status.clone(),
            published_port: c.published_port,
            // The old `runs::ContainerInfo` never tracked a container's
            // network-internal IP — only `Action::ContainerObserved` (a
            // real Docker-polling effect's future report) will ever set
            // this for real.
            ip: None,
            ports: c.ports.clone(),
            sync: match c.synced {
                Some(true) => SyncStatus::Synced,
                Some(false) => SyncStatus::Drifted,
                None => SyncStatus::Unknown,
            },
        },
        // Always `None`: every call site translating a `runs::ContainerInfo`
        // this way is itself the moment an in-flight action just settled
        // (or a fresh wire-time snapshot, which never has one in flight
        // either) — never a mid-flight observation.
        pending_action: None,
    }
}

fn mirror_route(r: &runs::PortRoute) -> PortRoute {
    PortRoute {
        domain: r.domain.clone(),
        host_port: r.host_port,
        wildcard: r.wildcard,
        container_port: r.container_port.clone(),
        https: r.https,
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
    outcome: Option<Result<RunState, String>>,
}

impl CreateSettleGuard {
    fn settle(mut self, outcome: Result<RunState, String>) {
        self.outcome = Some(outcome);
    }
}

impl Drop for CreateSettleGuard {
    fn drop(&mut self) {
        let actor = self.actor.clone();
        let run_id = std::mem::take(&mut self.run_id);
        let outcome = self.outcome.take().unwrap_or_else(|| {
            Err(
                "run creation task ended without reporting a result (likely a panic or cancellation)"
                    .to_string(),
            )
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

/// The combined snapshot this effect converges on: both per-node
/// start/stop/delete jobs and whole-run create/top-up jobs, bundled
/// together so a single `Effect` (wired once per workspace, see
/// `daemon::WorkspaceRegistry::wire_actor`) drives both — the plan's target
/// module layout files both under `effects/docker/converge.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvergeSnapshot {
    pending: Vec<PendingEntry>,
    creates: Vec<PendingCreate>,
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
}

impl DockerConvergeEffect {
    pub fn new(old: Arc<server::WorkspaceState>, actor: ActorHandle) -> Self {
        Self {
            old,
            actor,
            in_flight: HashSet::new(),
            creates_in_flight: HashSet::new(),
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
        tokio::spawn(async move {
            let result = perform(&old, &entry)
                .await
                .map(|maybe_info| maybe_info.map(|info| mirror_container(&info)))
                .map_err(|e| format!("{e:#}"));
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
        tokio::spawn(async move {
            let result = perform_create(&old, &entry)
                .await
                .map_err(|e| format!("{e:#}"));
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn old_container(node_id: &str, status: &str) -> runs::ContainerInfo {
        runs::ContainerInfo {
            node_id: node_id.to_string(),
            container_name: format!("fghj-{node_id}-1"),
            status: status.to_string(),
            published_port: Some(54321),
            domain: format!("{node_id}.fghj.internal"),
            raw_domain: format!("{node_id}.fghj.raw.internal"),
            routes: vec![runs::PortRoute {
                domain: format!("{node_id}.fghj.internal"),
                host_port: 54321,
                wildcard: false,
                container_port: "80".to_string(),
                https: true,
            }],
            additional_hosts: vec![],
            ports: BTreeMap::from([("80".to_string(), Some(54321u16))]),
            status_port: Some("80".to_string()),
            config_hash: "hash".to_string(),
            synced: None,
            pending_action: None,
        }
    }

    fn old_run(run_id: &str, containers: Vec<runs::ContainerInfo>) -> runs::RunState {
        runs::RunState {
            run_id: run_id.to_string(),
            network: format!("fghj-net-{run_id}"),
            containers,
            sidecar_container_name: format!("fghj-sidecar-{run_id}"),
            sidecar_ip: Some("172.20.0.2".to_string()),
        }
    }

    #[test]
    fn mirror_runs_carries_over_run_and_container_identity() {
        let mirrored = mirror_runs(&[old_run("default", vec![old_container("web", "running")])]);
        let run = &mirrored["default"];
        assert_eq!(run.network, "fghj-net-default");
        assert_eq!(run.sidecar_ip.as_deref(), Some("172.20.0.2"));
        let container = &run.containers["web"];
        assert_eq!(container.desired.raw_domain, "web.fghj.raw.internal");
        assert_eq!(container.observed.status, "running");
        assert_eq!(container.observed.ports["80"], Some(54321));
        assert!(container.pending_action.is_none());
        assert!(run.pending_create.is_none());
    }

    #[test]
    fn mirror_runs_marks_a_running_container_as_desired_running() {
        let mirrored = mirror_runs(&[old_run("default", vec![old_container("web", "running")])]);
        assert!(mirrored["default"].containers["web"].desired.running);
    }

    #[test]
    fn mirror_runs_marks_a_stopped_container_as_not_desired_running() {
        let mirrored = mirror_runs(&[old_run("default", vec![old_container("web", "exited")])]);
        assert!(!mirrored["default"].containers["web"].desired.running);
    }

    #[test]
    fn mirror_runs_maps_synced_flag_to_sync_status() {
        let mut synced = old_container("web", "running");
        synced.synced = Some(true);
        let mut drifted = old_container("api", "running");
        drifted.synced = Some(false);
        let mirrored = mirror_runs(&[old_run("default", vec![synced, drifted])]);
        assert_eq!(
            mirrored["default"].containers["web"].observed.sync,
            SyncStatus::Synced
        );
        assert_eq!(
            mirrored["default"].containers["api"].observed.sync,
            SyncStatus::Drifted
        );
    }

    #[test]
    fn mirror_runs_handles_a_container_with_no_published_ports() {
        let mut unpublished = old_container("db", "starting");
        unpublished.ports = BTreeMap::from([("5432".to_string(), None)]);
        let mirrored = mirror_runs(&[old_run("default", vec![unpublished])]);
        assert_eq!(
            mirrored["default"].containers["db"].observed.ports["5432"],
            None
        );
    }

    #[test]
    fn mirror_runs_covers_multiple_runs_and_containers() {
        let mirrored = mirror_runs(&[
            old_run("default", vec![old_container("web", "running")]),
            old_run("other", vec![old_container("api", "running")]),
        ]);
        assert_eq!(mirrored.len(), 2);
        assert!(mirrored["default"].containers.contains_key("web"));
        assert!(mirrored["other"].containers.contains_key("api"));
    }

    #[test]
    fn mirror_runs_of_an_empty_snapshot_is_empty() {
        assert!(mirror_runs(&[]).is_empty());
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
