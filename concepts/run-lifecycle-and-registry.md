# Environment lifecycle and the orchestrator

> **Where state lives.** `orchestrator::Orchestrator` holds no environment
> state. It owns the Docker client, the database handle, the per-node
> lifecycle locks and the log-capture tasks — the machinery that *does*
> things. The workspace's one `EnvState` lives in the workspace actor, and
> everything below reports its results back there as `Action`s. See
> [[state-and-effects]] for the full picture and for what the second copy
> this struct used to keep actually cost.

## One environment per workspace

A workspace has at most one environment: one network, one sidecar, one
container per node (`state::WorkspaceState::env: Option<EnvState>`). There
used to be *named runs* — `POST /runs` with a `run_id` stood up a second,
fully separate copy of the environment. They were removed because they
could not do the one thing their UI advertised. A named run varied identity,
volume contents and which nodes were up, but not the source
([[branch-ownership-model]] removed the per-run branch pin) and not the
configuration, so "review a PR in a second run" was impossible by
construction. What was left — a scratch copy with virgin volumes — did not
justify a run id threaded through every type, action, route and table.

`run_id` survives in exactly one place: as a private key fixed to
`orchestrator::DEFAULT_RUN_ID` in Docker names and labels (`fghj.run`), in
the SQLite tables and in the events/logs API, so that existing containers,
volumes and databases keep their names. Nothing above `orchestrator/` and
`persistence/` sees it.

## One start path

Everything that creates a container goes through
`Orchestrator::bring_up`, behind two entry points:

- **`ensure_running(graph, spec, prior, progress)`** — "bring the
  environment up to date for `spec`." `EnvSpec::Flow(flow)` is a flow
  switch: every running container outside the flow is stopped first
  (dependents before what they depend on — stopped, not removed, so
  switching back reuses the same containers and volumes), then everything
  the flow needs is topped up. `EnvSpec::Node(id)` is the first start in a
  workspace with no environment yet: the node and what it can't start
  without. Either way it creates the network and sidecar if they don't
  exist yet, and leaves alone any container that is already alive *and still
  matches its config*. Liveness is checked against Docker on every call
  (`docker::inspect_status`), not trusted from `EnvState` — a container can
  be stopped or removed out-of-band between calls.

  "Already alive" is necessary but not sufficient to skip a node —
  `orchestrator::orchestrate::top_up_may_skip` holds the three conditions, and the
  third is that the node's config hasn't drifted since it was started. A
  top-up is an explicit user action, and the expectation people bring to one
  is `compose up`'s: my edits take effect. Note the deliberate asymmetry with
  the reconciler below, which sees the same drift verdict and never acts on
  it — see [[config-drift]] for why "who asked" is the distinction that
  matters. A node a top-up recreates also restarts the alive nodes that
  can't start without it, since they may hold connections to the container
  that went away.

- **`restart_node(graph, node_id, prior)`** — the Drawer's **Start** /
  **Recreate** button on a node in an existing environment, and what applies
  a debug-switch flip. Same path, in "restart this node" mode: the node
  itself is always recreated (so a `.fghj.yaml` edit is picked up and a
  `run: once` task re-runs); what it can't start without is started only if
  it isn't up, and drift is *not* a reason to bounce one — the user asked
  to start one node, not to apply every pending edit behind it; and the
  running services that can't start without it are restarted afterwards
  (`restart_targets`). A dependent failing to come back is logged and
  reported as its own state, but doesn't fail the Start.

There used to be a third path: a per-node cascade hand-rolled in
`effects::docker::converge::perform` (`needs_start` for requirements,
`restart_container` for the node, a `dependents_of` loop after), with its
own skip rules, its own health budget and per-node locking the create path
didn't have. Two paths that are supposed to agree on "what does starting
this node involve" eventually don't.

Every container start takes that node's lock (→ [[concurrency-model]]).

### Failure keeps what came up

A node failing does not roll anything back. Tearing down the three
containers that came up because the fourth didn't would destroy exactly the
progress the per-node reporting (`orchestrator::progress`) exists to keep,
in the one environment everything else in the workspace shares. Only what
can't start without the failed node is skipped (`StartOutcomes`); everything
else still comes up.

"Keep what came up" is only half a policy, though. The containers are also
the reducer's to know about, or they are running, reachable from inside the
Docker network, and absent from the UI and from host routing —
`state::query::resolve_route` reads reducer state alone. So the error
carries the progress: `state::EnvCreateError` is
`{ message, partial: Option<EnvState> }`, `ensure_running` fills `partial`
at the point of failure, and the reducer adopts it.

Blocking what can't start without a failed node is also what gives a
**terminating node** its teeth: a task (a seed, a migration) is not considered started until its
container has exited 0, and it is ordered before the service that owns it, so
a failed migration leaves everything downstream of it unstarted rather than
running against an unmigrated database. See [[terminating-nodes]] — including
why a finished task deliberately reads as `desired.running == false` and so
drops out of the liveness top-up entirely.

### Blocking warnings refuse the start

Both entry points resolve a fresh graph first, and `Graph::refuse_if_blocked`
runs before either of them is called (`effects::docker::converge::perform_create`
for a create/top-up, `perform`'s `Starting` arm for a single node — starting one
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
= 1s, matched to the frontend's own `/environment` poll interval so the UI is
essentially never stale) that calls
`effects::docker::observe::report` for every wired workspace. That reads the
actor's current state, hands it to `Orchestrator::inspect_containers`, and
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
answers into a second copy of the environment state and persist that, which is how
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
is constructed, against whatever environment was persisted from a previous
`fghjd` lifetime. The granularity is **per container, not per
environment**: each persisted container is inspected, the ones Docker still
knows about are kept with a freshly-read `observed.status`, the ones it
doesn't are dropped, and only an environment left with *nothing* alive is
deleted from the db.

That distinction was a real bug, not a style preference. Dropping the whole
environment's tracked list on a single miss — one container stopped, renamed, or
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

Each `ContainerInfo` the environment tracks carries a `routes: Vec<PortRoute>` —
`{domain, host_port}` pairs built by `start_node` from every port that's
either `primary` or `name`d (see [[node-identity-and-domains]] for how the
domain itself is derived). This is what
`state::query::resolve_route` — reached through `daemon::routing`'s
`RouteResolver` impl for `WorkspaceRegistry` (→ [[local-ca-and-tls-proxy]])
— actually scans to turn an
incoming HTTPS SNI into a `127.0.0.1:<port>` to relay to — `fghjd` runs on
the host, outside the Docker network, so it can't rely on Docker's own
embedded per-network DNS the way sibling containers can; `routes` is the
bridge. `ContainerInfo.routes` is persisted alongside the rest of the environment's
state (see [[persistence-and-workspace-store]]), so routing survives a
`fghjd` restart along with everything else `persistence::rehydrate`
reconciles.

## Status

Implemented: `src/orchestrator/` (`Orchestrator::ensure_running`/`restart_node`/
`stop`/`inspect_containers`, route derivation).
`daemon::spawn_reconciler` drives the inspection on a 1s tick. Known gap:
`resolve_route` filters on `status == "running"`, which
excludes stopped containers but can briefly still return a route to a
container that's been fully *removed* between reconcile ticks.
