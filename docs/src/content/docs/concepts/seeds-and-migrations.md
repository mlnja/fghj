---
title: Seeds and migrations
description: Why a migration is waited on through its store, and a seed runs only on data it hasn't filled — keyed on the volumes, not the container.
---

## The hole this fills

[terminating nodes](/concepts/terminating-nodes/) made a seed expressible: a task, ordered after its
database, gating what needs it. It left *when it runs* to `run:`, and both
answers are wrong for a seed:

- `run: on_start` runs it every start, so it is only safe if the seed is
  idempotent. A seed that drops and reloads a database (aikido-core's
  `seed-database.sh`) isn't, and shouldn't have to be.
- `run: once` skips it on a clean exit of a container fghj can still see.
  That is evidence about the *container*. A seed container removed while the
  volume stays reseeds on top of the data; a volume wiped while the container
  stays leaves an empty database marked done.

So in practice the seed was typed by hand, through `fghj exec`.

## What everyone who gets this right does

| Tool | What says "already done" | Keyed on |
|---|---|---|
| postgres/mysql images, `/docker-entrypoint-initdb.d` | the data directory is empty | the data |
| Flyway, dbmate, Rails/Django migrations | a ledger table in the database | the data |
| Rails `db:prepare`, Supabase `db reset` | seeded when created; reseed is a reset | the data |
| Compose `service_completed_successfully` | nothing; it runs on every `up` | — |
| Helm `post-install` hook | once per release | the orchestrator's lifecycle |

The record of "done" belongs with the data. The ones keyed on an
orchestrator's lifecycle, `run: once` included, go wrong the moment a volume
outlives a container or the other way round.

## Two kinds, two keywords, one form each

A migration writes the schema, and a seed writes data. They differ in every
way that matters:

| | `migrates: mysql` | `seeds: [mysql, minio]` |
|---|---|---|
| runs | every run that includes mysql, once it's up | once its stores are up and migrated, only on new data |
| who waits | everything that waits on mysql, without saying so | nobody |
| contract | idempotent, as every migration runner already is | none: it never runs over data it already filled |
| on failure | mysql isn't ready: what needs it is blocked | only the seed fails |

Both are ordinary services, so image, build, env, volumes and working
directory mean what they mean everywhere. A migration or a seed nested
*inside* its store was the other shape, and it was rejected: the tool is
rarely in the store's image (dbmate lives in the app's image, mysqldef in
its own), so a nested form needs a second container spec inside a service —
a second build context, env file and working directory, resolved against a
service that doesn't own them. A seed can fill several stores, so it can't
nest under one anyway.

A store is a service in the same repo, and not a task: a store stays up. One
migration per store, so the order of two can't be a guess. Neither declares
`run:`, which the keyword replaces.

## Waiting on a store waits on its migration

`add_store_edges` adds, for every edge needed to start whose target is a
migrated store, an edge from the same dependent to the migration, marked
`implied`. `php` waits on `mysql`, so `php` waits on `migrate`. The seed
waits on `mysql`, so the seed comes after `migrate` too: `mysql → migrate →
seed`, the order Rails uses.

The alternative was asking each consumer to also wait on the migration.
Everyone who forgets starts against an old schema, which is the bug this
exists to remove. The cost is an edge nobody wrote, so the graph doesn't draw it. A
migration has no card of its own: it's the `migrate` segment of its store's
status bar, which opens it. Waiting on the store is waiting on it, and the
edge to the store already says so.

Seeds get no such edge. Most services start against an unseeded database —
the schema is there, the rows aren't — and making them wait would make every
start as slow as the seed and a broken seed take the app down with it. A
service that really needs the data says so: `depends_on: {seed: {condition:
service_completed_successfully}}`. A skipped seed counts as completed.

That holds across repos too. Waiting on another repo's flow waits on each of
its members except a seed, so `depends_on: [backend/base]` draws no line to
backend's seed. A flow that starts the waiter, with `required` left on, still
runs the seed.

Because nothing waits on a seed, a seed can depend on the app it seeds
through — POSTing fixtures to an API — without a cycle. A migration can't:
it would wait on something waiting on it, which `check_cycles` refuses.

## A migration starts with its store

`graph::add_migrations` adds the migration of every store in a start: a
flow's members (in `resolve_flows`), a single node's start, and a restart's
targets. Listing `mysql` in a flow brings its migration, and a flow never
names `migrate`. A restart of the store reruns the migration even if it last
exited cleanly — a recreated store may hold anything.

A seed is a step in a journey, so it starts only when a flow lists it.
The graph draws it as a card of its own, tinted and striped so it doesn't
read as a service, and hides seeds unless the **seeds** checkbox above the
graph is ticked.

## What "already seeded" means

After a seed exits 0, fghj records the data it ran against
(`seeds` table: node id, data, time). Before running it again, it reads the
data again and skips the seed if it's the same, narrating "skipped: mysql
still holds the data this seeded 3 days ago" as the start's only step.

The data of a store is each volume its container mounts, named or
anonymous, as `name@created` (`docker::data_identity`). Creation time is the
part that matters: a volume removed and made again under the same name is
new data. A store with no volume at all keeps its data in the container, so
there it's the container id. Several stores are recorded together; new data
in any of them reruns the seed, whose job is stores consistent with each
other.

- **A failed seed is never recorded**, so it always runs again.
- **A store that can't be read runs the seed**, rather than skipping on a
  guess.
- **The Start button on a seed always runs it.** It's the escape hatch for a
  seed you know is safe to rerun.
- **Data in a bind mount isn't seen.** A host directory has no creation time
  Docker reports, so such a store reads as its container; a recreated
  container reseeds. Use a named volume for data a seed fills.
- **The record lives in `.fghj/fghj.db`**, created with `IF NOT EXISTS`
  outside the schema version, because a version bump drops every table and
  this one is worth keeping across an upgrade.

## Not done yet

- **Reset data.** Reseeding means removing the store's volumes by hand. The
  intended action stops the stores and what uses them, removes their volumes,
  and starts again — the seed then runs because the data is new.
- **"The seed changed since this data was seeded."** That needs a hash of the
  files the seed reads, not of the commit, which moves on every pull and
  would warn constantly.
- **Retiring `run: once`.** It's the container-keyed version of `seeds:`;
  it stays until nothing uses it.
