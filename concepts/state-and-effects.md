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
  happen. Requests (`EnvPlanned`, `NodeStartRequested`, `NodeStopRequested`,
  `NodeDeleteRequested`, `NodeDebugWaitRequested`) and reports
  (`ContainerObserved`, `ContainerActionSettled`, `EnvCreateWorking`,
  `EnvCreateProgress`, `EnvCreateSettled`, `ConfigDriftObserved`,
  `VolumeObserved`, `OwnerSet`).
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

That rule was learned the expensive way. For a while there were two stores:
the actor's, and the orchestrator's own map of run states (`RunRegistry`, as
it was then), kept roughly in step by `effects::bridge` re-polling the
latter once a second. Every lifecycle call had to remember
to write both. What that bought:

- Observations could reach the database ahead of the reducer that was
  supposed to own them, because `refresh` wrote into `Orchestrator`'s copy
  and `Orchestrator` persisted its copy.
- Config drift was written into a second `synced` field on `Orchestrator`'s
  container as well as into `ContainerObserved::sync`, and the two
  disagreed.
- A `pending` map in `Orchestrator` gated duplicate actions in parallel with
  the reducer's `pending_action`, so "is this node already starting?" had
  two answers.

The second store was then deleted outright. `Orchestrator` owns only the
Docker-facing *machinery* — the bollard client, the database handle, the
per-node lifecycle locks, the log-capture tasks — and holds no environment
state at all. Where it used to read its own map, it now takes what it needs
as an argument and reports what happened back as an `Action`:

| Then | Now |
|---|---|
| `start(graph, spec)` read prior state from its own map | `ensure_running(graph, spec, prior, progress)` |
| `refresh()` inspected Docker and wrote into its own map | `inspect_containers(&env) -> BTreeMap<node_id, observed>` |
| `config_drift(graph)` walked its own map | `config_drift(graph, &env)` |
| `stop(run_id)` looked the run up in its own map | `stop(&env)` |
| `list()` was how the actor got seeded | `server::WorkspaceState::rehydrated`, read once |
| `commit(run_id, apply)` merged concurrent per-node writes | the reducer; it is the only writer |

Persistence and the sidecar route table stopped being something each
lifecycle call had to remember to do at the end, and became what they
always were: pure functions of published state, `effects::persist` and
`effects::routes`.

## Reporting progress without `orchestrator/` knowing about actors

Removing the inline `save_run` from the create path opened a real hole: a
daemon that died halfway through bringing up a five-node environment would leave
five containers running and nothing written down — fghj manufacturing the
very `Orphaned` state its observer exists to surface
([[run-lifecycle-and-registry]]).

The fix is `orchestrator::progress`: `ensure_running` takes an optional
`ProgressSink` (`mpsc::UnboundedSender<RunProgress>`) and reports each node
as work on it begins and the moment it comes up. `effects::docker::converge`
owns the receiving end and translates each report into
`Action::EnvCreateWorking` / `Action::EnvCreateProgress`.

The same reports feed the activity log ([[docker-and-downloads]]), which is
what the UI shows as a workspace's history of switches, starts and stops.
Each `pending_create` opens a top-level job — `switch` for a flow, `start`
for a node — and each `pending_action` one of its action's kind (`start`,
`stop`, `delete`). The run reports its plan first (`RunProgress::Planned`:
what it may start, in start order, and what it will stop), and every node in
it but the job's own becomes a queued child; a node the plan didn't list
becomes one when it is first reported. Each is settled by the node's own done
or failed report. A planned node the run left alone (already current) gets
no report, so once the run ends the effect settles it as done if it is up, or
had nothing to do; after a failure, one that isn't up was blocked, and the
job's settle cancels it. The job itself settles with the effect's result. Opening a job supersedes a
running one under the same key (it is cancelled, then replaced), and the
handle is dropped through a guard, so an aborted effect reads as cancelled
rather than running forever. The log is only a record: the reducer, not it,
decides what runs.

It is a plain channel rather than an `ActorHandle` on purpose: `orchestrator/`
knows how to talk to Docker and nothing about actors, actions, or reducers.
Handing it an `ActorHandle` would invert that and make the orchestration
layer depend on the state layer it is supposed to be driven by.

Ordering matters and is explicit: `spawn_create` drops its sender and
`await`s the drain task **before** the settle guard fires, so a late
progress report can never land after `EnvCreateSettled` and re-add a
container the settle just dropped.

## Tearing the environment down is not an action

There is no teardown action. The only thing that removes an environment is
removing its workspace (`daemon::WorkspaceRegistry::remove`, behind
`POST /workspaces/remove`), which marks it in `removing.json`, deregisters
the actor and aborts its converge task, and only *then* starts the teardown
job (`daemon::removal`). The order is the point: once the actor is gone there
is no state left for a late `EnvCreateProgress` or `EnvCreateSettled` to
put a container back into.

The teardown doesn't read the actor's environment at all. It lists the
workspace's containers by their `com.docker.compose.project` label and
removes them, sweeping again in case a start the abort cut short still lands
one, then the sidecar, the routes directory and the network. Every step can
run again, so a fghjd that dies mid-teardown just runs it again on start: an
id in `removing.json` is dropped from the index and never loaded.

This replaced `RunStopRequested` / `RunTeardownSettled`, which had to guard
every later report against resurrecting a run the teardown had removed
(`EnvCreateProgress` used `get_mut`, not `entry`; `EnvCreateSettled`
checked presence first) — rules each new report would have had to remember.

Known gap: aborting the converge task does not abort a create *it already
spawned*, so a create in flight when the workspace is unwired keeps doing
Docker work against a network `stop` is removing.

## What this does *not* remove

Per-node locks are still real and still needed (→ [[concurrency-model]]).
The reducer's `pending_action` dedup rejects a *second request* for a node
already in flight; the locks serialize the Docker calls themselves, which
also race against paths the reducer never saw. Those are different jobs
and collapsing them is what created the two-answer `pending` map above.

## Status

Implemented. `src/actor.rs`, `src/action.rs`, `src/reducer/`,
`src/effects/`, `src/state/`. `Orchestrator` is stateless;
`server::WorkspaceState` keeps `rehydrated` (what a previous
`fghjd` lifetime left running, reconciled against Docker once at
construction) purely to seed the actor in
`daemon::WorkspaceRegistry::wire_actor`.

`effects::bridge` and `Action::WorkspaceMirrored` are gone. No effect acts on
`observed`. Bringing a crashed service back is `daemon::heal`, which reads it
and dispatches an ordinary `NodeStartRequested`, so a restart is a request
like any other and passes the reducer's busy gate. A deleted container waits
for the user — see [[run-lifecycle-and-registry]].
