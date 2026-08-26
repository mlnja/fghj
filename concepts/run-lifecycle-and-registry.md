# Run lifecycle and the run registry

## Two different verbs, one shared registry

`RunRegistry` (`src/runs.rs`) has two entry points that look similar but
mean different things, and picking the wrong one would either destroy work
unnecessarily or silently fail to start what the user asked for:

- **`start(graph, spec)`** — "make exactly this run exist, from scratch."
  If a run with this `run_id` is already up, it's stopped and torn down
  first (`already_running` check), then every non-flow-filtered node in the
  graph is started fresh. This is what a **named/review run** always goes
  through: `POST /runs` with a non-empty `run_id` (`daemon::post_runs`
  routes on `spec.run_id.is_some()`). A review run is meant to be
  reproducible from a clean slate every time you hit "start" again — e.g.
  after changing which branch is overridden.
- **`ensure_running(graph, flow)`** — "top up the one shared default
  environment so everything reachable from `flow` (or the whole graph, if
  `flow` is `None`) is running; never touch a container that's already
  alive." This backs both "Start default environment"
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

`daemon::post_runs` is the single HTTP entry point that decides which of
these two to call, purely based on whether the request specified a
`run_id`.

## Branch overrides: a throwaway build, never the live checkout

A `RunSpec.overrides` entry (`{node_id: branch}`) only affects how
`start_node` decides what to build **from**:

- **No override** (the common case): builds straight from the live
  workspace checkout at `node.local_path`, tagged
  `fghj/{sanitized-id}:{sanitized-branch-or-"local"}` — so local edits are
  picked up on every run without needing a commit first.
- **Override present**: builds from a **throwaway mirror + checkout** under
  `<workspace>/.fghj/`, never touching the live checkout. `ensure_mirror`
  (`resolver.rs`) clones (or reuses) a bare `--mirror` of the repo so
  `git show <branch>:fghj.yaml`-style access to any branch works without a
  full checkout per branch; `docker::materialize_checkout` then produces an
  actual working tree for the requested branch. See
  [[branch-ownership-model]] for the full rationale — this ephemeral,
  side-by-side override is deliberately the *only* way to build a specific
  branch of a dependency, precisely because there is exactly one live
  checkout per repo, workspace-wide.

## The reconciler: read-only drift correction

`daemon::spawn_reconciler` runs a background loop (every `RECONCILE_INTERVAL`
= 1s, matched to the frontend's own `/runs` poll interval in `App.svelte` so
the UI is essentially never stale) that calls `RunRegistry::refresh` for
every wired workspace. `refresh` re-inspects each live run's containers
against real Docker state and writes back any status that changed —
including flagging a container that's vanished entirely (`docker rm`'d by
hand, outside fghj) as `"removed"`. This is explicitly analogous to a
Kubernetes controller's reconcile loop, but **read-only**: it never
recreates, restarts, or otherwise "heals" anything. If a container dies, the
UI will show that honestly on the next tick rather than fghj silently
bringing it back — the user decides whether to restart it. The lock over
`RunRegistry`'s in-memory state is deliberately never held across an
`.await`: `refresh` snapshots container names while holding it, does all the
(async) Docker inspection without it, then re-locks only to write results
back.

## Reconciling at startup, not just steady-state

`RunRegistry::new` runs the same kind of check once, at daemon startup,
against whatever runs were persisted from a previous `fghjd` lifetime: a run
whose containers are *all* still alive is restored with freshly-inspected
statuses; a run missing even one container (removed out-of-band, or lost
across a reboot with no restart policy configured) is dropped outright
rather than being presented to the UI as a run that's only partially there.

## `ContainerInfo.routes`: how the proxy finds a container

Each `ContainerInfo` a run tracks carries a `routes: Vec<PortRoute>` —
`{domain, host_port}` pairs built by `start_node` from every port that's
either `primary` or `name`d (see [[node-identity-and-domains]] for how the
domain itself is derived). This is what
`daemon::WorkspaceRegistry::resolve_route`
(→ [[local-ca-and-tls-proxy]]'s `RouteResolver`) actually scans to turn an
incoming HTTPS SNI into a `127.0.0.1:<port>` to relay to — `fghjd` runs on
the host, outside the Docker network, so it can't rely on Docker's own
embedded per-network DNS the way sibling containers can; `routes` is the
bridge. `ContainerInfo.routes` is persisted alongside the rest of a run's
state (see [[persistence-and-workspace-store]]), so routing survives a
`fghjd` restart along with everything else `RunRegistry::new` reconciles.

## Status

Implemented: `src/runs.rs` (`RunRegistry::start`/`ensure_running`/`stop`/
`refresh`, branch-override build path, route derivation).
`daemon::spawn_reconciler` drives `refresh` on a 1s tick. Known gap (see
`PROGRESS.md`): `resolve_route` filters on `status == "running"`, which
excludes stopped containers but can briefly still return a route to a
container that's been fully *removed* between reconcile ticks.
