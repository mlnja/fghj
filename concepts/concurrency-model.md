# The concurrency model

What may happen at the same time, what may not, and what protects each.

`fghjd` is one process serving many workspaces, and within a workspace many
nodes. Almost everything it does is I/O against Docker, so almost every
interesting operation is `async` and holds state across an `.await`. That is
where the rules matter.

## The units

There are three nested scopes, and each behaves differently:

| scope | may run concurrently with itself? | guarded by |
|---|---|---|
| workspace | yes — different workspaces share nothing but the Docker daemon | nothing |
| environment (`EnvState`) | no — there is one, and only its actor writes it | the workspace actor |
| node (one container) | **no** — serialized per node | `Orchestrator::node_locks` |

Different workspaces are genuinely independent: separate networks, separate
sidecars, separate databases, separate route-table directories. Nothing in
`fghj` coordinates them, and nothing needs to.

## Per-node locks, not a workspace lock

Every container start (`bring_up`, behind both `ensure_running` and
`restart_node`), `stop_container`, and `remove_container` each take the lock
for **their node only** — `Orchestrator::node_lock(node_id)`, which hands out
a `tokio::sync::Mutex` created on first use and keyed by node id.

Two calls for the *same* node genuinely cannot interleave: each does
`docker::stop_and_remove` and then recreates a container with the same fixed
name, and Docker rejects the recreate for whichever loses the race. Two calls
for *different* nodes have no such conflict — different containers, different
volumes, different route entries.

This used to be one workspace-wide `action_lock`, which conflated the two.
Because these calls are held across `start_node`, and `start_node` waits on
the node's healthcheck, a single slow or unhealthy node could block every
other node's Start and Stop button in the workspace for the length of that
wait. The lock was doing real work; it was just doing it at the wrong
granularity.

The locks are `tokio::sync::Mutex` because they are held across `.await`. The
map that hands them out is a plain `std::sync::Mutex`, held only for the
lookup and never across an await — that distinction is the rule for every
mutex in the registry.

Serializing is *all* the node lock does. Rejecting a genuine duplicate — a
second "start" for a node that is already starting — happens upstream in the
reducer, off `ContainerInfo::pending_action`. The registry deliberately keeps
no second gate on the same fact, because two gates can disagree.

## Writes merge; they do not clobber

Finer locking makes a lost-update hazard that was previously masked. Every
lifecycle path used to clone the whole run state, mutate the clone across a
long stretch of Docker work, and write the clone back wholesale. With one
workspace-wide lock that was merely wasteful. With per-node locks it would
lose data: two nodes starting concurrently would each write back a snapshot
taken before the other's container existed, and whichever finished second
would erase the first.

The fix went through two shapes. First an `Orchestrator::commit` that
re-read the live state under a mutex and applied a targeted change in place.
Now the orchestrator holds no state at all: lifecycle calls *return* what
they did — a container, an `EnvCreateProgress` per node — and the workspace
actor's reducer is the only thing that writes `EnvState`. Each report is
applied to the current state as a targeted change, so two nodes reporting
concurrently both land. Persistence, the route table and DNS are effects
derived from the published state ([[state-and-effects]]), so none of them
can render from a snapshot that has already lost a sibling's routes.

## Waiting is bounded per run, not per node

`wait_for_healthy` polls a container's declared healthcheck. It was always
bounded per node, but nodes start *sequentially*, so a per-node bound bounded
nothing a person waiting on a terminal actually cares about: an N-node run
worst-cased at N × 120 s with no ceiling.

The unit that is bounded is therefore the whole run. `HealthBudget` carries a
deadline shared by every node in one `start` / `ensure_running` call. Each
node's allowance is *whatever is left, capped at the per-node limit* — the
cap so that one slow node cannot consume the entire run's allowance, the
remainder so that the run as a whole terminates. A node Start
(`restart_node`) goes through the same path, so it gets the same budget,
shared with whatever requirements and dependents it brings up alongside.

Running out truncates the **wait**, not the run. This is the same
best-effort call `wait_for_healthy` already made at its own limit: containers
still get created, later nodes simply stop waiting for health first.
Refusing to bring up the rest of an environment because a clock ran out would
turn a slow start into no environment at all, which is strictly worse than a
slow one.

A truncated wait is recorded distinctly in the node's event stream rather
than logged as a clean pass. A node fghj never actually saw report healthy
must not look identical to one it did — that difference is exactly what
someone debugging a flaky start needs to see.

A run actually carries **two** such deadlines, side by side in `RunBudget`.
The second bounds waits on terminating nodes (`kind: "task"` — seeds,
migrations), and it is deliberately not the same clock: running out of the
health budget means fghj stops waiting and carries on, while running out of
the task budget *fails* the task and blocks everything downstream of it. One
shared deadline would let a slow healthcheck early in a run escalate into a
hard failure somewhere else entirely. See [[terminating-nodes]].

## What is *not* guarded

Stated plainly, because these are choices and not oversights:

- **Two workspaces racing on Docker itself.** Network and container names are
  workspace-qualified, so they do not collide; host port allocation is left to
  Docker. Nothing serializes them.
- **Two whole-environment starts.** Not a gap any more. The reducer rejects
  a second `EnvPlanned` while `pending_create` is set, and every node action
  while one is in flight, so there is never more than one create per
  workspace. `bring_up` takes each node's lock around its `start_node`
  anyway, which costs nothing and keeps the per-node rule unconditional.
- **Out-of-band Docker changes.** Someone stopping a container by hand races
  with everything. That is what the observer path (`refresh`, `SyncStatus`)
  exists to notice after the fact — see [[run-lifecycle-and-registry]].

## Where this lives

`src/orchestrator/registry.rs` (`node_locks`, `node_lock`) ·
`src/orchestrator/lifecycle.rs` (the three per-node commands) ·
`src/orchestrator/health.rs` (`HealthBudget`, `RunBudget`, `wait_for_healthy`,
`HealthOutcome`, `wait_for_exit`, `TaskOutcome`) ·
`src/orchestrator/orchestrate.rs` (one budget per whole-run call)

A version of this page rewritten for a reader who has not read `src/` lives
at `docs/src/content/docs/concepts/concurrency-model.md`; this one stays the
durable record.
