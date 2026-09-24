# State and effects: one store, one writer

## The shape

Everything `fghjd` knows about a workspace lives in one value —
`state::WorkspaceState` — owned by one task, the workspace **actor**
(`src/actor.rs`). Nothing else holds a copy. Changing it takes exactly one
path:

```
Action  ->  reducer::reduce(&state, action) -> Result<WorkspaceState, ActionRejected>
                                    |
                                    v
                        actor publishes the new state
                                    |
                                    v
                Effects observe it and make the world match
```

- **`Action`** (`src/action.rs`) — the complete vocabulary of things that can
  happen. Requests (`RunPlanned`, `RunNodeStartRequested`, `RunStopRequested`)
  and reports (`ContainerObserved`, `ContainerActionSettled`,
  `RunCreateProgress`, `RunCreateSettled`, `RunTeardownSettled`,
  `ConfigDriftObserved`, `VolumeObserved`).
- **`reducer::reduce`** (`src/reducer/`) — a pure function. No I/O, no clock,
  no Docker. It decides, it never acts. Rejecting an action here
  (`ActionRejected`) is how a duplicate or nonsensical request is refused,
  and it is refused *before* anything touches Docker.
- **The actor** applies reduce serially and publishes the result over a
  `tokio::sync::watch`. Serial application is the whole concurrency story
  for state: there is no lock to forget to take, because there is nothing
  to lock.
- **Effects** (`src/effects/`) subscribe to that watch and are responsible
  for making some part of the outside world match what they see:
  containers (`docker::converge`), the SQLite store (`persist`), the
  sidecar route table (`routes`), `/etc/hosts` (`hosts`), the OS resolver
  (`dns`), `pf` NAT rules (`raw_net`).

## Why an effect is `extract` + `converge`, not one function

```rust
pub trait Effect: Send {
    type Snapshot: PartialEq + Clone + Send;
    fn extract(&self, state: &WorkspaceState) -> Self::Snapshot;
    fn converge(&mut self, snapshot: Self::Snapshot) -> anyhow::Result<()>;
}
```

The split exists so the **driver**, not each effect, owns the "did anything
actually change?" check. `run_effect` extracts, compares against the last
snapshot, and only calls `converge` when they differ. Two consequences that
are the point of the design:

- An effect can't accidentally re-apply itself on every unrelated state
  change. `/etc/hosts` is not rewritten because a container's published
  port moved.
- `extract` *is* the effect's declaration of what it depends on. If
  `DockerConvergeEffect::extract` reads only `desired` and never
  `observed`, then no observation can ever make it act — that is how
  "observe drift, don't heal it" is enforced structurally rather than by
  everyone remembering the rule. Flipping it to self-healing means
  changing `extract`, in one place, deliberately.
- `last` is deliberately *not* updated when `converge` fails, so a failed
  convergence is retried on the next state change rather than being
  recorded as done.

`AsyncEffect` is the same contract with `converge` returning a future (RPITIT),
for effects whose work is genuinely async — `persist` writes to SQLite.

## The single-store rule

The one invariant this architecture exists to protect: **every fact has
exactly one home.** Two copies of a fact is two chances for them to
disagree, and reconciling them is a poller, and a poller is a window in
which the system is confidently wrong.

That rule was learned the expensive way. Through migration phase 4 there
were two stores: the actor's, and `runs::RunRegistry`'s own
`BTreeMap<String, RunState>`, kept roughly in step by `effects::bridge`
re-polling the latter once a second. Every lifecycle call had to remember
to write both. What that bought:

- Observations could reach the database ahead of the reducer that was
  supposed to own them, because `refresh` wrote into `RunRegistry`'s copy
  and `RunRegistry` persisted its copy.
- Config drift was written into a second `synced` field on `RunRegistry`'s
  container as well as into `ContainerObserved::sync`, and the two
  disagreed.
- A `pending` map in `RunRegistry` gated duplicate actions in parallel with
  the reducer's `pending_action`, so "is this node already starting?" had
  two answers.

Phase 5 deleted the second store outright. `RunRegistry` now owns only the
Docker-facing *machinery* — the bollard client, the database handle, the
per-node lifecycle locks, the log-capture tasks — and holds no run state
at all. Where it used to read `self.runs`, it now takes what it needs as an
argument and reports what happened back as an `Action`:

| Then | Now |
|---|---|
| `start(graph, spec)` read prior state from `self.runs` | `start(graph, spec, prior, progress)` |
| `refresh()` inspected Docker and wrote into `self.runs` | `inspect_containers(&runs) -> Vec<(run_id, observed)>` |
| `config_drift(graph)` walked `self.runs` | `config_drift(graph, &runs)` |
| `stop(run_id)` looked the run up in `self.runs` | `stop(run_id, &state)` |
| `list()` was how the actor got seeded | `server::WorkspaceState::rehydrated`, read once |
| `commit(run_id, apply)` merged concurrent per-node writes | the reducer; it is the only writer |

Persistence and the sidecar route table stopped being something each
lifecycle call had to remember to do at the end, and became what they
always were: pure functions of published state, `effects::persist` and
`effects::routes`.

## Reporting progress without `runs/` knowing about actors

Removing the inline `save_run` from the create path opened a real hole: a
daemon that died halfway through bringing up a five-node run would leave
five containers running and nothing written down — fghj manufacturing the
very `Orphaned` state its observer exists to surface
([[run-lifecycle-and-registry]]).

The fix is `runs::progress`: `start`/`ensure_running` take an optional
`ProgressSink` (`mpsc::UnboundedSender<RunProgress>`) and report each node
the moment it comes up. `effects::docker::converge` owns the receiving end
and translates each report into `Action::RunCreateProgress`.

It is a plain channel rather than an `ActorHandle` on purpose: `runs/`
knows how to talk to Docker and nothing about actors, actions, or reducers.
Handing it an `ActorHandle` would invert that and make the orchestration
layer depend on the state layer it is supposed to be driven by.

Ordering matters and is explicit: `spawn_create` drops its sender and
`await`s the drain task **before** the settle guard fires, so a late
progress report can never land after `RunCreateSettled` and re-add a
container the settle just dropped.

## Nothing resurrects a torn-down run

A create and a teardown can overlap — the user presses Stop while a
five-node top-up is still on node three. `RunTeardownSettled(Ok)` removes
the run from the map, which is exactly what `effects::persist` and
`effects::routes` key off to clean up the database row and the sidecar's
route directory. So every action that arrives *after* it must not put the
run back:

- `RunCreateProgress` uses `get_mut`, not `entry`.
- `RunCreateSettled` checks presence before inserting, on both the success
  and the partial-failure branch.

`RunPlanned` created the entry, so writing through it is always correct
when the run is still supposed to exist. `reducer::observation`'s
`a_create_that_settles_after_its_run_was_torn_down_does_not_resurrect_it`
covers all three arms.

## What this does *not* remove

Per-node locks are still real and still needed (→ [[concurrency-model]]).
The reducer's `pending_action` dedup rejects a *second request* for a node
already in flight; the locks serialize the Docker calls themselves, which
also race against paths the reducer never saw. Those are different jobs
and collapsing them is what created the two-answer `pending` map above.

## Status

Implemented. `src/actor.rs`, `src/action.rs`, `src/reducer/`,
`src/effects/`, `src/state/`. `RunRegistry` is stateless as of migration
phase 5; `server::WorkspaceState` keeps `rehydrated` (what a previous
`fghjd` lifetime left running, reconciled against Docker once at
construction) purely to seed the actor in
`daemon::WorkspaceRegistry::wire_actor`.

`effects::bridge` and `Action::WorkspaceMirrored` are gone. Relatedly:
`effects::docker::policy::DockerHealPolicy` exists and is deliberately
unused — it is the switch that would make `converge` act on `observed`,
and leaving it off is the "read-only reconciliation" decision in
[[run-lifecycle-and-registry]].
