---
title: Terminating nodes
description: Why seeds and migrations run to completion as tasks, what makes a service one, and why it isn't a clever restart policy.
---

## The hole this fills

Every other node denotes a **long-running process**. Your own service and
a stock database image both mean "start it and keep it up", and the only
ordering primitive across them is a healthcheck — a predicate gated on a
container reporting Docker-`healthy`, which a container that has exited can
never do.

So before tasks existed, a database seed could not be a graph node at
all. It couldn't be ordered against the database it seeds, it couldn't gate
the service that needs it, it wasn't recorded, and it didn't re-run. The only
mechanism available was `fghj exec`: a full-duplex interactive relay that
needs an already-running container *and a live human at a keyboard*. In
practice you typed the seed command by hand after the environment came up,
every time, and nothing in the system knew it had happened.

## Why fghj has to know

The decisive argument is one observation: **`observed.status == "exited"`
means opposite things for the two.** For a service it's drift — the thing
fghj was asked to keep up is not up. For a migration it's success.

Nothing downstream of the reducer holds the resolved graph. The drift
reconciler, the UI's status badge, `ensure_running`'s "is this still alive?"
check, and every state projection all read container state and nothing else.
A field on the *config* wouldn't reach any of them; only a distinction the
runtime state itself carries can. That's `ContainerDesired::terminating`, and
the resolver marking a node as a task is the one place it's ever decided.

The same observation rules out expressing a task as a service with a clever
`restart` policy or `healthcheck`. A task has neither field, because a
restart policy on a container whose purpose is to exit would restart it
forever, and a container that has exited can never report healthy.

## The language

A task is an ordinary entry in `services:`. What makes it a task is either
the thing Docker Compose uses to say "this is meant to exit" (something
requires it with `condition: service_completed_successfully`) or declaring
`run:`.

```yaml
services:
  api:
    build: .
    depends_on:
      db: {condition: service_healthy}
      migrate: {condition: service_completed_successfully}
  migrate:
    build: .                     # the same code as api
    command: ["./bin/migrate"]
    depends_on:
      db: {condition: service_healthy}
  db:
    image: postgres:16
    healthcheck:
      test: ["CMD", "pg_isready"]
```

There's no `kind:` to write, and no way for two readings to disagree: a
waiter's condition, or the task's own `run:`, is the statement that it's
meant to exit.

- **Waiting on a task any other way is a blocking warning.** A second
  service writing `depends_on: [migrate]` would be expecting it to stay up.
  fghj refuses that start rather than pick one reading.
- **`command` is required and non-empty.** A task *is* its command. One
  without would run the image's default `CMD` — typically the service's own
  long-running entrypoint — and never exit, hanging the run until the task
  budget expired.
- **Your own code is `build: .`**, like the service it migrates. It's built
  under its own node's tag; the second build is a cache hit, since both
  build the same context.
- **A task has no healthcheck and no restart policy.** Declaring either is a
  blocking warning.
- **A task nothing waits on declares `run:`.** A seed only one journey needs
  has `run: on_start`, its own `depends_on` (on the database it seeds), and is
  listed in that journey's flow. It starts once what it needs is up, and its
  clean exit reads as success.

Waiting on another repo with `service_completed_successfully` is refused: a
flow is waited on until it's up, and never completes. A task is always its
own repo's business.

## Ordering

`depends_on` edges are the only ordering, as for every other node: for every
edge, `to` is the dependency and `from` the dependent. The example above
orders as `db → migrate → api`.

Ordering alone would be decoration, though. What makes it load-bearing is
that a task is not considered *started* until it has **finished**: fghj waits
for the container to exit, and a task that exits non-zero — or never exits —
fails the node. A failed node blocks everything that requires it,
transitively, so a failed migration keeps the service from coming up against
an unmigrated database. The rest of the run keeps starting, and the run then
reports what failed and what it blocked. See
[Dependency kinds](/concepts/dependency-kinds/).

A cycle of required `depends_on` edges is a blocking warning. It has to be: the topological
start order deliberately *cannot* fail (on a cycle it appends the leftovers
in stable sorted order, so a run still starts something), so a cycle would
otherwise pass silently and produce an arbitrary order.

## Re-run policy: `on_start` by default

`run: on_start` (the default) re-runs the task on every start and every
top-up. `run: once` skips it — but only on the evidence of a clean exit fghj
can still see.

The default is the safe half of the tradeoff, so it's the one you get without
asking: *skipping* is the answer that can be silently wrong. `on_start` makes
idempotence the task author's contract — which is what a migration runner
already provides — rather than making correctness depend on fghj guessing
whether the work is still needed.

**`once` is deliberately not keyed on the config hash.** It could be: the
spec hash covers the checkout's HEAD commit, so a `git pull` that adds a
migration does move it (see [config sync in tutorial chapter
7](/tutorial/06-when-it-goes-wrong/)). It still shouldn't be. `once` means "at most once per run", and an author reaches
for it exactly when re-running the task is expensive or destructive. "New code
arrived" is not a reason to re-run something they marked as unsafe to re-run
— if it were, they'd have left it `on_start`.

The evidence is the container itself and nothing else. Rehydration on daemon
startup drops any container Docker no longer has, so a task whose container
was removed out-of-band runs again rather than being assumed finished on the
strength of a record of it. And `once` means "succeed once", not "be
attempted once": a task that exited non-zero is never treated as done, or the
service behind it would be permanently blocked with no way to retry.

## Two budgets, not one

[Concurrency model](/concepts/concurrency-model/) describes the run-wide
health budget: one deadline for a whole run's health waits, because nodes
start sequentially and a per-node limit bounded nothing a waiting human cares
about.

A task wait cannot share that deadline, because the two answer differently
when they run out. A spent **health** budget means fghj stops waiting and
carries on — the container is up either way, and refusing to start the rest of
an environment over a slow healthcheck is strictly worse than a truncated
wait. A spent **task** budget *fails the task*, and a failed task blocks
everything downstream. Sharing one deadline would let a slow Postgres
healthcheck earlier in the run fail an unrelated migration that hadn't even
started waiting yet — a best-effort wait silently escalating into a hard
failure somewhere else.

So the two deadlines sit side by side, each bounded by the same run-wide and
per-node limits. Both branches read "was the budget already spent?" *before*
waiting, not after: waiting to a deadline exhausts the budget by definition,
so asking afterwards would report every genuine timeout as a wait that never
happened.

## What the state carries

| Field | Means |
|---|---|
| `desired.terminating` | This container's desired terminal state is "exited 0", not "running". |
| `desired.running` | `false` for a task **even on success** — `running` records what fghj wants at rest, and what it wants for a finished task is for it to stay finished. |
| `observed.exit_code` | The outcome for a task; one more detail of a crash for a service. |

`running: false` is the load-bearing half. Every state projection filters on
a container Docker reports as `running`, so a finished task drops out of route
resolution, DNS zones, and raw-net NAT without any of them needing to know
the kind exists. Had `running` stayed `true`, a completed migration would read
as permanent drift and the proxy would keep hunting for a route into a
container that's supposed to be gone.

Both fields persist, so a task survives a `fghjd` restart as a task rather
than reading back as a service that crashed.

## In the UI

A task gets its own kind icon, and its status bar reads on a different scale:
**completed** (quiet success — there's nothing there to reach, so it must not
read as "live") or **failed**, instead of the running/stopped pair a service
gets.

## Related

- [Run lifecycle & registry](/concepts/run-lifecycle-and-registry/) — `start`
  vs. `ensure_running`, and the reconciler a terminating node deliberately
  drops out of.
- [Concurrency model](/concepts/concurrency-model/) — the health budget this
  one is modelled on.
- [Node identity & domains](/concepts/node-identity-and-domains/) — a task's
  id is the same `name.owning-service-id` shape a backing dependency gets.
- [Tutorial chapter 3](/tutorial/03-a-migration/) — the same idea, worked
  through against a real Postgres.
