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
| run (`RunState`) | no, by convention — `start` stops an existing run first | — |
| node (one container) | **no** — serialized per node | `RunRegistry::node_locks` |

Different workspaces are genuinely independent: separate networks, separate
sidecars, separate databases, separate route-table directories. Nothing in
`fghj` coordinates them, and nothing needs to.

## Per-node locks, not a workspace lock

`restart_container`, `stop_container`, and `remove_container` each take the
lock for **their node only** — `RunRegistry::node_lock(run_id, node_id)`,
which hands out a `tokio::sync::Mutex` created on first use and keyed by the
pair.

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
lifecycle path used to clone the whole `RunState`, mutate the clone across a
long stretch of Docker work, and write the clone back wholesale. With one
workspace-wide lock that was merely wasteful. With per-node locks it would
lose data: two nodes starting concurrently would each write back a snapshot
taken before the other's container existed, and whichever finished second
would erase the first.

So state changes go through `RunRegistry::commit`, which re-reads the live
`RunState` under the registry mutex and applies a **targeted** change in
place. It hands back the merged result, and that merged value — never the
caller's earlier snapshot — is what gets passed to `save_run` and
`write_route_table`. Otherwise the route table could publish a snapshot that
had already lost a sibling's routes, which is worse than a stale database
row: the sidecar would stop routing a container that is running fine.

`commit` returns `None` when the run has disappeared while the caller was
working. That is the honest answer — there is nothing left to update, and
re-inserting the run would resurrect an environment the user just stopped.

## Waiting is bounded per run, not per node

`wait_for_healthy` polls a container's declared healthcheck. It was always
bounded per node, but nodes start *sequentially*, so a per-node bound bounded
nothing a person waiting on a terminal actually cares about: an N-node run
worst-cased at N × 120 s with no ceiling.

The unit that is bounded is therefore the whole run. `HealthBudget` carries a
deadline shared by every node in one `start` / `ensure_running` call. Each
node's allowance is *whatever is left, capped at the per-node limit* — the
cap so that one slow node cannot consume the entire run's allowance, the
remainder so that the run as a whole terminates. A single-node
`restart_container` shares its budget with nothing and gets the full per-node
limit.

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

## What is *not* guarded

Stated plainly, because these are choices and not oversights:

- **Two workspaces racing on Docker itself.** Network and container names are
  workspace-qualified, so they do not collide; host port allocation is left to
  Docker. Nothing serializes them.
- **`start` and `ensure_running` against each other.** Neither takes a node
  lock, because each owns the whole run. Calling both concurrently on one run
  is a caller error the registry does not defend against.
- **Out-of-band Docker changes.** Someone stopping a container by hand races
  with everything. That is what the observer path (`refresh`, `SyncStatus`)
  exists to notice after the fact — see [[run-lifecycle-and-registry]].

## Where this lives

`src/runs/registry.rs` (`node_locks`, `node_lock`, `commit`) ·
`src/runs/lifecycle.rs` (the three per-node commands) ·
`src/runs/health.rs` (`HealthBudget`, `wait_for_healthy`, `HealthOutcome`) ·
`src/runs/orchestrate.rs` (one budget per whole-run call)
