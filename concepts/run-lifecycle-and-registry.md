# Run lifecycle and the run registry

> **Where state lives.** `RunRegistry` holds no run state. It owns the
> Docker client, the database handle, the per-node lifecycle locks and the
> log-capture tasks — the machinery that *does* things. Every `RunState` in
> the daemon lives in the workspace actor, and everything below reports its
> results back there as `Action`s. See [[state-and-effects]] for the full
> picture and for what the second copy this registry used to keep actually
> cost.

## Named runs have no UI affordance, on purpose

`RunControls.svelte` used to carry a `+ Review run` button and a name field.
Both are gone. The run list and its per-run **Stop** stay, so a run that
exists is still visible and still stoppable, and `POST /runs` still takes a
`run_id` — nothing about the mechanism changed.

What changed is the promise. A named run varies exactly three things:
identity (network, container names, run-qualified domains), the contents of
its non-`stable` volumes, and which of its nodes are up. It does not vary
the source — [[branch-ownership-model]] removed the per-run branch pin — and
it does not vary the configuration, because the config→run map is a constant
function ([[AUDIT]] E2, open). So two runs stood up from one workspace are
necessarily identical code with identical config.

That makes "review a PR in a second run" the one thing the feature cannot
do, and it was exactly what the button's label, the placeholder
(`review-auth-pr123`) and a numbered tutorial chapter all advertised. The
honest remaining use is a scratch environment with virgin volumes: point
something destructive at it and keep the environment you were working in.
That is a real use, and a narrow one, so it is documented
(`docs/.../reference/runs.md`, deliberately not in the sidebar) rather than
offered in the primary controls.

Closing E2 — per-run environment overrides, source identical, config
different — is what would earn the feature a promotion back. It carries
neither the mirror-clone permission problem nor the different-graph problem
that killed the branch pin, because the graph does not move.

## Two different verbs, one shared registry

`RunRegistry` (`src/runs/`) has two entry points that look similar but
mean different things, and picking the wrong one would either destroy work
unnecessarily or silently fail to start what the user asked for:

- **`start(graph, spec, prior, progress)`** — "make exactly this run exist,
  from scratch."
  If a run with this `run_id` is already up, it's stopped and torn down
  first (`already_running` check), then every non-flow-filtered node in the
  graph is started fresh. This is what a **named run** always goes
  through: `POST /runs` with a non-empty `run_id` (`daemon::post_runs`
  routes on `spec.run_id.is_some()`). A named run is meant to be
  reproducible from a clean slate every time you hit "start" again.
- **`ensure_running(graph, flow)`** — "top up the one shared default
  environment so everything reachable from `flow` (or the whole graph, if
  `flow` is `None`) is running; leave alone any container that is already
  alive *and still matches its config*." This backs both "Start default
  environment"
  (`RunControls.svelte`'s `startDefault`) and "Run flow"
  (`Header.svelte`'s `runFlow`, on the Actual tab) — both are just different
  scopes of the same idempotent top-up. `fghj` models **one** shared set of
  running containers per workspace under `runs::DEFAULT_RUN_ID`, not a
  separate environment per flow, so picking a flow to run must never
  restart (or duplicate) whatever's already up because some *other* flow
  needed it too. Liveness is checked directly against Docker on every call
  (`docker::inspect_status`), not trusted from the persisted `RunState` —
  a container can be stopped/removed out-of-band between calls (see the
  reconciler, below), so only a fresh check is trustworthy. State is saved
  to the DB after *every* node started, not just at the end, so a failure
  partway through a multi-node top-up doesn't lose track of the containers
  that did start successfully.

  "Already alive" is necessary but not sufficient to skip a node —
  `runs::orchestrate::top_up_may_skip` holds the three conditions, and the
  third is that the node's config hasn't drifted since it was started. A
  top-up is an explicit user action, and the expectation people bring to one
  is `compose up`'s: my edits take effect. Note the deliberate asymmetry with
  the reconciler below, which sees the same drift verdict and never acts on
  it — see [[config-drift]] for why "who asked" is the distinction that
  matters.

`daemon::post_runs` is the single HTTP entry point that decides which of
these two to call, purely based on whether the request specified a
`run_id`.

### They also fail differently, and that is deliberate

`start` rolls back: a node failing partway removes every container it
started, the sidecar and the network. A named run is meant to be
reproducible from a clean slate, and a half-built one is garbage nobody
asked for.

`ensure_running` does the opposite — it keeps what came up. Tearing down the
three containers that started because the fourth didn't would destroy exactly
the progress the per-node `save_run` above exists to preserve, in the one
environment everything else in the workspace shares.

The trap is that "keep what came up" is only half a policy. The containers were
also persisted to SQLite and to the sidecar route table, but the *reducer* never
heard about them, because only the success path returned a `RunState`. They were
running, reachable from inside the Docker network, and absent from `GET /runs`,
from the UI, and from host routing — `state::query::resolve_route` reads reducer
state alone. Two stores of truth disagreeing, with nothing to reconcile them.

So the error carries the progress: `state::RunCreateError` is
`{ message, partial: Option<RunState> }`, `ensure_running` fills `partial` at
the point of failure, and the reducer adopts it. `start` leaves it `None` —
it genuinely has nothing outstanding. One type, two honest answers.

Stopping the loop on a node error is also what gives a **terminating node**
its teeth: a task (a seed, a migration) is not considered started until its
container has exited 0, and it is ordered before the service that owns it, so
a failed migration leaves everything downstream of it unstarted rather than
running against an unmigrated database. See [[terminating-nodes]] — including
why a finished task deliberately reads as `desired.running == false` and so
drops out of the liveness top-up entirely.

### Blocking warnings refuse the start

Both entry points resolve a fresh graph first, and `Graph::refuse_if_blocked`
runs before either of them is called (`effects::docker::converge::perform_create`
for a whole run, `perform`'s `Starting` arm for a single node — starting one
node still reads the whole graph, so a blocking problem anywhere is a blocking
problem here too).

The reason the check lives *here* and not in the resolver is that resolution
must never fail. A workspace you cannot fully understand still has to be
viewable, or you lose the screen that would show you what is wrong with it. So
`resolve_universe` always returns a graph, every finding is a
`resolver::Warning` carrying a `Severity`, and the refusal happens at the one
place that stops being a view and starts being an actuation.

`Blocking` means "this instruction cannot be carried out as written" — a
dependency on a service that does not exist, two `primary` ports, a
self-dependency, a dangling `shared-backing` reference, two nodes whose derived
names collide. Starting anyway would bring up a subset of the environment
nobody asked for and report success. `Advisory` means the config describes
something inert rather than something wrong: a repo with no services, a
`wildcard` port with no domain to wildcard, a dependency cycle.

Cycles are advisory on purpose. `topological_start_order` already handles one
without hanging — when no node can make progress it appends the remainder in
sorted order and starts anyway — and that fallback is better than refusing to
bring up an environment over a loop. What was missing was that nobody was told:
the start order quietly stops being a dependency order, a service comes up
before the database it needs, and the symptom is an unexplained crash loop.
`resolver::cycles` now names the loop (`a -> b -> c -> a`) so the crash loop has
an explanation attached.

The refusal lists every blocking warning, not the first. They are usually
independent mistakes, and discovering them one start attempt at a time is
miserable.

One known coarseness: a `Warning` carries a message and a severity, not the
node it is about, so the single-node path refuses too when the blocking problem
is in an unrelated repo. On one small shared workspace that is the safe
direction to err in, but attributing warnings to node ids would let the
single-node gate be as narrow as the action is.

## Branch overrides: removed

`start_node` always builds straight from the live workspace checkout at
`node.local_path`, tagged `fghj/{sanitized-id}:{sanitized-branch-or-"local"}`
— so local edits are picked up on every run without needing a commit first.

This used to have a second mode: a `RunSpec.overrides` entry (`{node_id:
branch}`) would build that node from a throwaway mirror + checkout under
`<workspace>/.fghj/` instead, via `resolver::ensure_mirror` +
`docker::materialize_checkout`, without touching the live checkout. It was
removed — see [[branch-ownership-model]]'s per-run branch pin note for why
(a real, structural permission bug in the mirror clone, plus no designed
answer for an override branch whose own `.fghj.yaml` doesn't match the live
graph's shape).

## The reconciler: read-only drift correction

`daemon::spawn_reconciler` runs a background loop (every `RECONCILE_INTERVAL`
= 1s, matched to the frontend's own `/runs` poll interval in `App.svelte` so
the UI is essentially never stale) that calls
`effects::docker::observe::report` for every wired workspace. That reads the
actor's current state, hands it to `RunRegistry::inspect_containers`, and
dispatches what Docker actually says back as `Action::ContainerObserved` —
including flagging a container that's vanished entirely (`docker rm`'d by
hand, outside fghj) as `"removed"`. This is explicitly analogous to a
Kubernetes controller's reconcile loop, but **read-only**: it never
recreates, restarts, or otherwise "heals" anything. If a container dies, the
UI will show that honestly on the next tick rather than fghj silently
bringing it back — the user decides whether to restart it.

Note the direction: `inspect_containers` *reports*, it does not write.
`observed` is the reducer's to record, and the reducer is the only thing
that records it. That was not always true — the inspection used to fold its
answers into a second copy of the run state and persist that, which is how
an observation could reach SQLite ahead of the state machine that owned it
(→ [[state-and-effects]]).

## When a node disappears

Read-only reconciliation has a case a Kubernetes controller never has to
handle: the *desired* side can vanish. A `git switch` to a branch whose
`.fghj.yaml` doesn't declare `bff` leaves a `bff` container running, holding
its ports, its domain and its volumes, with nothing in the freshly-resolved
graph to compare it against.

`SyncStatus` names this outright — `Orphaned`, alongside `Synced`, `Drifted`
and `Unknown`. The distinction that matters is `Orphaned` vs. `Unknown`:
`Unknown` means *fghj has nothing to say* (no drift check has run yet, or
re-resolving the spec failed), while `Orphaned` is a positive finding about the
world. Collapsing the two — which is what `DriftReport` carrying an
`Option<bool>` used to force — makes a vanished service look exactly like a
freshly-started one nobody has inspected, which is the one reading guaranteed
to be wrong.

Policy stays where the rest of this guide puts it: **nothing acts on it.** An
orphan is not stopped, removed, or reconciled. Stopping a container because a
branch moved would be a surprise, and a branch switch is frequently temporary.
What the observation buys is that the user is *told* — and the Actual tab
synthesizes a graph node for an orphaned container precisely so it stays
selectable, and therefore stoppable, after the declaration that created it is
gone. Without that, the container would be running and unreachable from the only
UI that can stop it.

Drift is keyed on the resolved spec, which for a built node now includes the
checkout's HEAD commit and its dirty bit — so a new commit on the same branch
*is* `Drifted`, even though the image tag `fghj/<id>:<branch>` is unchanged.
See [[config-drift]] for the whole hash and what it still misses.

### The other vanishing side: asked to be up, observed down

An orphan is the *desired* side disappearing. The mirror case is the desired
side being perfectly intact and the observed side falling away:
`desired.running == true` alongside an `exited` (or `dead`, or `removed`)
observation. It is reachable in the most ordinary way possible — the process
crashed, or someone ran `docker rm` — and, consistent with everything above,
**nothing converges it**. Only an explicit Start does.

That needed the same treatment `Orphaned` got, for the same reason: read-only
by default only works if the user is *told*, and for a long time this pair
rendered identically to a container the user had deliberately stopped. Both
said "stopped". One of those is a state somebody chose and the other is a
service that is down and staying down.

`state::NodeCondition` is the vocabulary — one derived word for what a
`desired`/`observed` pair means, computed by `ContainerInfo::condition` and
serialized alongside the pair itself so the UI reads it rather than
re-deriving it. `Crashed` is the variant the enum exists for. `Restarting`
and `Paused` are there for a smaller version of the same problem: both are
real Docker states, both mean the container is not serving (every route
lookup filters on `status == "running"`), and before this both read as
"stopped" too.

It is derived, never stored, and that is deliberate — a cached reading of two
fields sitting right next to it is a bug waiting for the one code path that
sets `status` without updating it. `ContainerInfo` has a hand-written
`Serialize` for exactly this reason, which is the only thing in the codebase
that is not `#[derive]`d there.

Note what did *not* change: nothing acts on `Crashed`. `AutoHeal` is still
constructed nowhere. The gap was never the policy, it was the vocabulary.

## Reconciling at startup, not just steady-state

`persistence::rehydrate` runs the same kind of check once, when a workspace
is constructed, against whatever runs were persisted from a previous
`fghjd` lifetime. The granularity is **per container, not per run**: each
persisted container is inspected, the ones Docker still knows about are
kept with a freshly-read `observed.status`, the ones it doesn't are
dropped, and only a run left with *nothing* alive is deleted from the db.

That distinction was a real bug, not a style preference. Dropping the whole
run's tracked list on a single miss — one container stopped, renamed, or
caught mid-recreate at exactly the moment `fghjd` restarted — silently
orphaned every *other* still-running container from `fghjd`'s bookkeeping,
including from the sidecar route table, which is built from exactly this
list. The containers kept running; fghj just stopped knowing about them.

Only `observed` moves during rehydration. What the persisted row said fghj
*wants* the container to be doing survives untouched, which is what lets a
container that died while `fghjd` was down come back reading as drifted
rather than as freshly correct. The result is parked on
`server::WorkspaceState::rehydrated` and read exactly once, to seed the
actor.

## `ContainerInfo.routes`: how the proxy finds a container

Each `ContainerInfo` a run tracks carries a `routes: Vec<PortRoute>` —
`{domain, host_port}` pairs built by `start_node` from every port that's
either `primary` or `name`d (see [[node-identity-and-domains]] for how the
domain itself is derived). This is what
`state::query::resolve_route` — reached through `daemon::routing`'s
`RouteResolver` impl for `WorkspaceRegistry` (→ [[local-ca-and-tls-proxy]])
— actually scans to turn an
incoming HTTPS SNI into a `127.0.0.1:<port>` to relay to — `fghjd` runs on
the host, outside the Docker network, so it can't rely on Docker's own
embedded per-network DNS the way sibling containers can; `routes` is the
bridge. `ContainerInfo.routes` is persisted alongside the rest of a run's
state (see [[persistence-and-workspace-store]]), so routing survives a
`fghjd` restart along with everything else `persistence::rehydrate`
reconciles.

## Status

Implemented: `src/runs/` (`RunRegistry::start`/`ensure_running`/`stop`/
`inspect_containers`, route derivation).
`daemon::spawn_reconciler` drives the inspection on a 1s tick. Known gap (see
`PROGRESS.md`): `resolve_route` filters on `status == "running"`, which
excludes stopped containers but can briefly still return a route to a
container that's been fully *removed* between reconcile ticks.
