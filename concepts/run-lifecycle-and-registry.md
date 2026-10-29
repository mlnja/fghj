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

`run_id` survives in exactly one place: as the fixed segment `default` in
Docker names (`orchestrator::naming`: `fghj-<workspace>-default-<node>`, and
the network that doubles as the Compose project label), in
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
  that went away. That's `restart_in_place`, a `docker restart` under
  `PendingAction::Restarting`: a recreate would rebuild them from the
  checkout and ship code nobody asked to ship.

- **`restart_node(graph, node_id, prior, progress)`** — the Drawer's **Start** /
  **Recreate** button on a node in an existing environment, and what applies
  a debug-switch flip. Same path, in "restart this node" mode: the node
  itself is always recreated (so a `.fghj.yaml` edit is picked up and a
  `run: once` task re-runs); what it can't start without is started only if
  it isn't up, and drift is *not* a reason to bounce one — the user asked
  to start one node, not to apply every pending edit behind it; and the
  running services that can't start without it are restarted in place
  afterwards (`restart_targets`), each reported through `progress` so the
  graph shows it restarting. A dependent failing to come back is logged and
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

## The reconciler: drift correction

`daemon::spawn_reconciler` runs a background loop (every `RECONCILE_INTERVAL`
= 1s, matched to the frontend's own `/environment` poll interval so the UI is
essentially never stale) that calls
`effects::docker::observe::report` for every wired workspace. That reads the
actor's current state, hands it to `Orchestrator::observe`, and
dispatches what Docker actually says back as `Action::ContainerObserved` —
including flagging a container that's vanished entirely (`docker rm`'d by
hand, outside fghj) as `"removed"`. This is explicitly analogous to a
Kubernetes controller's reconcile loop.

`observe` reads Docker with **one `list_containers` per workspace per tick**,
filtered on the environment's compose project label (every node and the
sidecar carry it), not an inspect per container: a big workspace was making
a dozen-plus calls a second. The listing carries status and published ports
but no exit code field, so the code is parsed from its status text
(`Exited (137) 5 seconds ago`); only an exited container whose text doesn't
carry one costs an inspect. The same listing says whether the sidecar is
running, and the reconciler restores it only on `SidecarSeen::Down`, so a
healthy sidecar costs no extra call.

Observing stays read-only; what it does with the reading is split by what
happened:

- **Docker not answering** (`Unknown`) — `observe` reports the
  last reading again with `observed.unseen` set. The status stays what it
  was, so routes, `/etc/hosts` and the raw-zone listeners don't churn on a blip, but the
  condition says nothing is known and heal leaves it alone.
- **Down without being asked** — `Interrupted` when it went down while
  fghjd couldn't watch (Docker unseen, or fghjd not running: the next fresh
  reading finds it down, see `ContainerObserved::settle_interruption`), so an
  engine stop or a reboot isn't reported as a crash. `Crashed` otherwise: a
  crash, a hand-typed `docker stop`. `daemon::heal` restarts both, logging
  which it was,
  by dispatching `NodeStartRequested`, the Start button's own request. The
  stored intent is the whole test: `desired.running` moves only when the user
  starts or stops through fghj, so anything down while it's `true` came from
  outside. One node per workspace at a time and only while nothing is in
  flight, because a start brings up its requirements and a parallel start of
  one of them would recreate it underneath. Backoff per node, 5s doubling to
  5 minutes, reset after a minute up; in memory, so a restarted `fghjd`
  starts over. Only while active: `fghj daemon stop` stops it too.

  The node tried longest ago goes first. A start can take longer than its
  backoff (a slow build), and picking the first due node by name let one
  failing build restart forever while every other crashed node waited
  behind it. After 8 restarts (about ten minutes) a node still down is left
  down, with a warning in the daemon log: by then it's a build or config
  failure that one more try won't fix, and each try holds up the rest. A
  Stop, or a Start by hand that stays up, makes it eligible again.

  Docker's own `restart: unless-stopped` was the alternative and is not
  used by default. When the engine comes back it starts everything at once,
  with no regard for dependency order or health, and nothing reaches the
  events pane. `restart` stays `"no"` unless a node asks otherwise.
- **Starting, called off** — a start is the one action another can take
  over. Stop and Delete are accepted while `pending_action` is `Starting`;
  converge sees the action change, aborts the start's task (a BuildKit solve
  on its own thread included: dropping it ends the session) and marks the
  start's open steps `cancelled` in its events. The aborted task's
  `SettleGuard` still reports, tagged `of: Starting`, and the reducer drops a
  settle whose action the node has moved on from. A start with no container
  yet (a placeholder) has nothing to stop: its entry just goes. Stop and
  Delete themselves are not interruptible, and a whole-flow create still
  holds every node until it settles.
- **Deleted** (`Missing`) — `docker rm`, a prune. Nothing can tell whether
  that was meant, so the user is asked: the Drawer offers **Recreate**
  (Start) or **Forget** (Delete, which is a no-op on Docker's side for a
  container that's already gone).

A Docker that doesn't answer is no reading at all: `observe` skips
the container rather than reporting it gone, so an outage doesn't show every
node as `Missing`.

Note the direction: `observe` *reports*, it does not write.
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
`desired.running == true` alongside an `exited` (or `dead`) observation. It
is reachable in the most ordinary way possible — the process crashed, or
the engine restarted — and `daemon::heal` restarts it, above.

That needed the same treatment `Orphaned` got, for the same reason: read-only
by default only works if the user is *told*, and for a long time this pair
rendered identically to a container the user had deliberately stopped. Both
said "stopped". One of those is a state somebody chose and the other is a
service that is down and staying down.

`state::NodeCondition` is the vocabulary — one derived word for what a
`desired`/`observed` pair means, computed by `ContainerInfo::condition` and
serialized alongside the pair itself so the UI reads it rather than
re-deriving it. `Crashed` is the variant the enum exists for. `Missing`
(status `"removed"`, whatever was asked) splits off the case where there is
no container left to start, so the UI can ask instead of offering Start.
`Restarting` and `Paused` are there for a smaller version of the same
problem: both are
real Docker states, both mean the container is not serving (every route
lookup filters on `status == "running"`), and before this both read as
"stopped" too.

It is derived, never stored, and that is deliberate — a cached reading of two
fields sitting right next to it is a bug waiting for the one code path that
sets `status` without updating it. `ContainerInfo` has a hand-written
`Serialize` for exactly this reason, which is the only thing in the codebase
that is not `#[derive]`d there.

Only `Crashed` and `Interrupted` are acted on, and only through the same
request a user would make. `Missing` waits for the user; `Unknown` waits for
Docker. A crash during an outage reads as `Interrupted`: fghjd didn't see
it, so it doesn't claim it.

## Reconciling at startup, not just steady-state

`persistence::rehydrate` runs the same kind of check once, when a workspace
is constructed, against whatever environment was persisted from a previous
`fghjd` lifetime. Each persisted container is inspected and **kept**: one
Docker still has gets a freshly-read status and exit code, one it doesn't
reads as `"removed"` with no exit code (so it shows as `Missing`, and a
`run: once` task whose container is gone runs again), and an unreachable
Docker keeps the record, marked unseen. One persisted as up and found down
reads as `Interrupted`: it went down while `fghjd` wasn't running.

Nothing is dropped, on purpose. It used to drop the containers it couldn't
find, and before that the whole environment on a single miss, which silently
orphaned every still-running container from `fghjd`'s bookkeeping and the
sidecar route table. Dropping also treated an *error* as "gone": `fghjd`
starting before Docker deleted the whole saved environment. A container
that's gone is now the user's call, like any other deletion.

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
`stop`/`observe`, route derivation).
`daemon::spawn_reconciler` drives the inspection on a 1s tick. Known gap:
`resolve_route` filters on `status == "running"`, which
excludes stopped containers but can briefly still return a route to a
container that's been fully *removed* between reconcile ticks.
