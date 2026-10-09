# Terminating nodes: seeds, migrations and one-shot jobs

> **Status:** implemented. Language in `schema/dependency.cue` (`#Task`) and
> `src/resolver/` (`TaskConfig`, `Dependency::Task`, `Node.kind == "task"`,
> `Node.run_policy`); runtime in `src/runs/health.rs`
> (`wait_for_exit`/`TaskOutcome`/`RunBudget`), `src/runs/start_node.rs`,
> `src/runs/node_spec.rs` and `src/runs/orchestrate.rs`; state in
> `state::ContainerDesired::terminating` and
> `state::ContainerObserved::exit_code`; rendering in
> `ui/src/lib/GraphView.svelte`. Closes [[AUDIT]] E1.

## The hole this fills

Before this, every node in the language denoted a **long-running process**.
`#Service` and `#BackingDependency` both mean "start it and keep it up", and
the only ordering primitive was `wait_for_healthy` — a predicate gated on a
container reporting Docker-`healthy`, which a container that has exited can
never do.

So a database seed could not be a graph node at all. It could not be ordered
against the database it seeds, it could not gate the service that needs it,
it was not recorded, and it did not re-run. The only mechanism that existed
was `fghj exec`: a full-duplex interactive relay that needs an
already-running container *and a live human*. In practice a developer typed
the seed command by hand after the environment came up, every time, and
nothing in the system knew it had happened.

## Why a kind, not a field

The decisive argument is a single observation: **`observed.status ==
"exited"` means opposite things for the two.** For a service it is drift — the
thing fghj was asked to keep up is not up. For a migration it is success.

Nothing downstream of the reducer holds the resolved graph. The drift
reconciler, the UI's status badge, `ensure_running`'s "is this still alive"
check and every projection in `state::query` all read `ContainerInfo` and
nothing else. A field on the *config* would not reach any of them; only a
distinction the runtime state itself carries can. That is
`ContainerDesired::terminating`, and `Node.kind == "task"` is the one place
it is ever decided.

The same observation rules out expressing a task as a service with a clever
`restart` policy or a `healthcheck`: a task may declare neither, because
a restart policy on a container whose purpose is to exit would restart it
forever, and a container that has exited can never report healthy.

## The language

A task is an ordinary `services:` entry. What makes it one is either
Compose's own idiom for a one-shot container, something in its repo requiring
it with `condition: service_completed_successfully`, or declaring `run:`
(`resolver::visit::mark_tasks`).

```yaml
services:
  api:
    build: .
    depends_on:
      db: {condition: service_healthy}
      migrate: {condition: service_completed_successfully}
  migrate:
    build: .                     # api's own code, another command
    command: ["./bin/migrate"]
    depends_on:
      db: {condition: service_healthy}
  db:
    image: postgres:16
    healthcheck: { test: ["CMD", "pg_isready"] }
```

- **The waiter's condition is the definition**, so the two readings can't
  disagree. Waiting on a task with any other condition is a blocking
  warning, and so is waiting on another repo with
  `service_completed_successfully` — a task is its own repo's business.
- **`command` is required and non-empty.** A task *is* its command. One
  without would run the image's default `CMD` — typically the service's own
  long-running entrypoint — and never exit, hanging the run until the task
  budget expired. The resolver is the enforcing boundary (see [[AUDIT]] B8).
- **`build: .` is the usual case**, since a migration is usually this
  repo's code with a different command. It builds under the task's own tag;
  Docker's layer cache makes the second build of the same context free
  (`runs::node_spec`).
- **A task has no healthcheck and no restart policy** — declaring either is
  blocking.
- **A task nothing waits on declares `run:`.** A seed only one journey needs
  has `run: on_start`, its own `depends_on`, and is listed in that journey's
  flow. Before `run:` made a task, such a container was read as a service and
  showed as crashed once it exited. See [[dependency-kinds]].

## Ordering

A task is ordered by `depends_on` edges like everything else: `to` is the
dependency and `from` the dependent (`runs::topological_start_order`). The
example orders as `db → migrate → api`.

Ordering alone would be decoration, though. What makes it load-bearing is
that `start_node` does not consider a task *started* until it has
**finished**: it waits for the container to exit, and a task that exits
non-zero — or never exits — fails the node. A failed node blocks everything
that requires it, transitively (`runs::order::StartOutcomes`), so a failed
migration keeps the service from coming up against an unmigrated database.
The rest of the run keeps starting; see [[dependency-kinds]].

A cycle of required `depends_on` edges is blocking (`resolver::cycles`). It has to be:
`topological_start_order` deliberately *cannot* fail (on a cycle it appends
the leftovers in stable sorted order so a run still starts something), so a
cycle would otherwise pass silently and produce an arbitrary order.

## Re-run policy: `on_start` by default

`run: on_start` (the default) re-runs the task on every start and every
top-up. `run: once` skips it, but only on the evidence of a clean exit fghj
can still see.

The default is the safe half of the tradeoff, so it is the one you get
without asking: skipping is the answer that can be silently wrong. `on_start`
makes idempotence the task author's contract — which is what a migration
runner already provides — rather than making correctness depend on fghj
guessing whether the work is still needed.

**`once` is deliberately not keyed on the config hash.** It could be:
`spec_hash` covers the checkout's HEAD commit, so a `git pull` that adds a
migration does move it ([[config-drift]]). It still should not be. `once`
means "at most once per run", and an author reaches for it exactly when
re-running the task is expensive or destructive. "New code arrived" is not a
reason to re-run something they marked as unsafe to re-run — if it were, they
would have left it `on_start`. The evidence is the container
itself and nothing else: `persistence::rehydrate` drops any container Docker
no longer has, so a task whose container was removed out-of-band runs again
rather than being assumed finished on the strength of a record of it. And
`once` means "succeed once", not "be attempted once" — a task that exited
non-zero is never treated as done, or the service behind it would be
permanently blocked with no way to retry.

## Two budgets, not one

[[concurrency-model]] describes the run-wide `HealthBudget`: one deadline for
a whole run's health waits, because nodes start sequentially and a per-node
limit bounded nothing a waiting human cares about.

A task wait cannot share that deadline, because the two answer differently
when they run out. A spent **health** budget means fghj stops waiting and
carries on — the container is up either way, and refusing to start the rest
of an environment over a slow healthcheck is strictly worse than a truncated
wait. A spent **task** budget *fails the task*, and a failed task blocks
everything downstream. Sharing one deadline would let a slow postgres
healthcheck earlier in the run fail an unrelated migration that had not even
started waiting yet — a best-effort wait silently escalating into a hard
failure somewhere else.

So `RunBudget` carries the two side by side, each bounded by the same
`RUN_LIMIT`/`PER_NODE_LIMIT` pair. Both branches read "was the budget already
spent?" *before* waiting, not after: waiting to a deadline exhausts the
budget by definition, so asking afterwards reports every genuine timeout as a
wait that never happened.

## What the state carries

| field | set by | means |
|---|---|---|
| `ContainerDesired::terminating` | `start_node`, from `Node.kind` | this container's desired terminal state is "exited 0", not "running" |
| `ContainerDesired::running` | `start_node` | `false` for a task **even on success** — `running` records what fghj wants at rest, and what it wants for a task at rest is for it to be finished |
| `ContainerObserved::exit_code` | `start_node`'s wait, and `inspect_containers` | the outcome for a task; one more detail of a crash for a service |

`running: false` is the load-bearing half. Every projection in
`state::query` filters on a container Docker reports as `running`, so a
finished task drops out of route resolution, DNS zones and raw-net NAT
without any of them needing to know the kind exists. Had `running` stayed
`true`, a completed migration would read as permanent drift and the proxy
would keep hunting for a route into a container that is supposed to be gone.

Both fields persist (`containers.terminating`, `containers.exit_code`), so a
task survives a `fghjd` restart as a task rather than reading back as a
service that crashed.

## In the UI

`GraphView.svelte` gives a task its own kind icon and reads its status bar on
a different scale: `completed` (quiet success — there is nothing there to
reach, so it must not read as "live") or `failed`, instead of the
`running`/`stopped` pair a service gets. `containerStateOf` is the single
place that branch lives.

## Related

- [[run-lifecycle-and-registry]] — `start` vs. `ensure_running`, and the
  reconciler a terminating node deliberately drops out of.
- [[state-and-effects]] — why `desired` and `observed` are separate, which is
  what makes "exited" representable as success rather than drift.
- [[concurrency-model]] — the health budget this one is modelled on.
- [[node-identity-and-domains]] — a task's id is the same
  `name.owning-service-id` shape a backing dependency gets.
- `docs/src/content/docs/tutorial/03-a-migration.md` — the same argument
  worked through against a real Postgres, for a reader who has not read `src/`.
  Its site-side counterpart of this file is
  `docs/src/content/docs/concepts/terminating-nodes.md`.
