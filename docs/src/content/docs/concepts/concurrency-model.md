---
title: Concurrency model
description: What may run at the same time and what may not — per-node locks, merging writes, and the run-wide health budget.
---

`fghjd` is one process serving many workspaces, and within a workspace many
nodes. Almost everything it does is I/O against Docker, so almost every
interesting operation is `async` and holds state across an `.await`. That's
where the rules matter.

## The units

Three nested scopes, each behaving differently:

| Scope | May run concurrently with itself? | Guarded by |
|---|---|---|
| Workspace | Yes — different workspaces share nothing but the Docker daemon | Nothing |
| Run | No, by convention — `start` stops an existing run first | — |
| Node (one container) | **No** — serialized per node | The registry's per-node locks |

Different workspaces are genuinely independent: separate networks, separate
sidecars, separate databases, separate route-table directories. Nothing in
fghj coordinates them, and nothing needs to.

## Per-node locks, not a workspace lock

Restarting, stopping, and removing a container each take the lock for **their
node only** — a `tokio::sync::Mutex` created on first use and keyed by
`(run_id, node_id)`.

Two calls for the *same* node genuinely cannot interleave: each stops and
removes the container, then recreates one with the same fixed name, and Docker
rejects the recreate for whichever loses the race. Two calls for *different*
nodes have no such conflict — different containers, different volumes,
different route entries.

This used to be one workspace-wide lock, which conflated the two. Because
these calls are held across node startup, and startup waits on the node's
healthcheck, a single slow or unhealthy node could block every other node's
Start and Stop button in the workspace for the length of that wait. The lock
was doing real work; it was just doing it at the wrong granularity.

The locks are `tokio::sync::Mutex` because they're held across `.await`. The
map that hands them out is a plain `std::sync::Mutex`, held only for the
lookup and never across an await — that distinction is the rule for every
mutex in the registry.

Serializing is *all* the node lock does. Rejecting a genuine duplicate — a
second "start" for a node that's already starting — happens upstream in the
reducer, off the node's `pending_action`. The registry deliberately keeps no
second gate on the same fact, because two gates can disagree.

## Writes merge; they do not clobber

Finer locking exposed a lost-update hazard that the coarse lock had masked.
Every lifecycle path used to clone the whole run state, mutate the clone
across a long stretch of Docker work, and write the clone back wholesale.
With one workspace-wide lock that was merely wasteful. With per-node locks it
would lose data: two nodes starting concurrently would each write back a
snapshot taken before the other's container existed, and whichever finished
second would erase the first.

So state changes go through a `commit` that re-reads the live run state under
the registry mutex and applies a **targeted** change in place. It hands back
the merged result, and that merged value — never the caller's earlier snapshot
— is what gets persisted and written to the route table. Otherwise the route
table could publish a snapshot that had already lost a sibling's routes, which
is worse than a stale database row: the sidecar would stop routing a container
that's running fine.

`commit` returns nothing to write when the run has disappeared while the
caller was working. That's the honest answer — there's nothing left to update,
and re-inserting the run would resurrect an environment the user just stopped.

## Waiting is bounded per run, not per node

Health waits poll a container's declared healthcheck. They were always bounded
per node, but nodes start *sequentially*, so a per-node bound bounded nothing
a person watching a terminal actually cares about: an N-node run worst-cased
at N × 120s with no ceiling.

The unit that's bounded is therefore the whole run. One health budget carries
a deadline shared by every node in a single `start` / `ensure_running` call.
Each node's allowance is *whatever is left, capped at the per-node limit* —
the cap so one slow node can't consume the entire run's allowance, the
remainder so the run as a whole terminates. A single-node restart shares its
budget with nothing and gets the full per-node limit.

Running out truncates the **wait**, not the run. Containers still get created;
later nodes simply stop waiting for health first. Refusing to bring up the
rest of an environment because a clock ran out would turn a slow start into no
environment at all, which is strictly worse than a slow one.

A truncated wait is recorded distinctly in the node's event stream rather than
logged as a clean pass. A node fghj never actually saw report healthy must not
look identical to one it did — that difference is exactly what someone
debugging a flaky start needs to see.

A run actually carries **two** such deadlines, side by side. The second bounds
waits on terminating nodes (tasks — seeds, migrations), and it's
deliberately not the same clock: running out of the health budget means fghj
stops waiting and carries on, while running out of the task budget *fails* the
task and blocks everything downstream of it. One shared deadline would let a
slow healthcheck early in a run escalate into a hard failure somewhere else
entirely. See [Terminating nodes](/concepts/terminating-nodes/).

## What is *not* guarded

Stated plainly, because these are choices and not oversights:

- **Two workspaces racing on Docker itself.** Network and container names are
  workspace-qualified, so they don't collide; host port allocation is left to
  Docker. Nothing serializes them.
- **`start` and `ensure_running` against each other.** Neither takes a node
  lock, because each owns the whole run. Calling both concurrently on one run
  is a caller error the registry doesn't defend against.
- **Out-of-band Docker changes.** Someone stopping a container by hand races
  with everything. That's what the observer path exists to notice after the
  fact — see
  [Run lifecycle & registry](/concepts/run-lifecycle-and-registry/).
