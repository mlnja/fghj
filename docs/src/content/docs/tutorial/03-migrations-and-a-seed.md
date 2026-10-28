---
title: 3. A migration
description: A task — a service that is supposed to exit — what makes something one, why "exited" needs two readings, and a migration versus a seed.
sidebar:
  order: 3
---

The database is up but empty. A schema migration is not like `web` or `db`:
it runs, it finishes, and then it is *correctly* not running. fghj calls
that a **task**.

## Why fghj has to know

For a service, `exited` is a problem. For a migration, `exited` is the
goal. Nothing downstream — the status badge, the reconciler, the "is this
node healthy" check — can tell those two apart without knowing which kind of
node it's looking at. See [Terminating nodes](/concepts/terminating-nodes/).

A task is any service that's meant to exit: something waits for it to
**complete successfully**, or it says which **store** it writes to. A
migration says the second thing.

## Declare it

First the SQL, in `storefront/schema.sql`:

```sql
create table if not exists orders (
  id    serial primary key,
  item  text        not null,
  price integer     not null,
  at    timestamptz not null default now()
);
```

Then a third service. `web` doesn't change:

```yaml
services:
  web:
    # ...as before
    depends_on:
      db: {condition: service_healthy}

  migrate:
    image: postgres:16
    command: ["psql", "-v", "ON_ERROR_STOP=1", "-f", "/schema.sql"]
    migrates: db
    environment:
      PGHOST: ${FGHJ_SERVICE_FQDN:db}
      PGUSER: shop
      PGPASSWORD: dev
      PGDATABASE: shop
    volumes:
      - host: ./schema.sql
        container: /schema.sql
        read_only: true

  db:
    # ...as before
```

Four things carry the weight here.

**`migrates: db`** is what makes `migrate` a task, and it does two more
things. `migrate` waits on `db` — healthy, since `db` has a healthcheck —
without a `depends_on` of its own. And everything that waits on `db` now
waits on `migrate` too, without saying so: `web` can't start against an old
schema because someone forgot a line. One migration per store, so the order
of two can't be a guess.

**`command` is required.** A task *is* its command. A container with no
command would just run the image's own long-running `CMD`, which is a
service.

**`image` here is the less common case.** A real migration is usually your
own code with another command (`rake db:migrate`, `alembic upgrade head`,
`npm run migrate`), so it would say `build: .` — the same build as `web`,
no second Dockerfile. We use an image only because our migration genuinely
isn't our code — it's stock `postgres:16` running `psql`.

**It runs on every start of `db`.** So its command has to be idempotent —
`create table if not exists` and `ON_ERROR_STOP=1` are both there for that.
Every real migration runner (dbmate, Flyway, Alembic, Rails) already is.

The bind mount puts your `schema.sql` inside the stock image. `host:
./schema.sql` resolves against this repo's checkout root, as every relative
path in this file does — the same rule Compose uses for paths relative to
the compose file.

## What the graph says

```json
{
  "id": "migrate.storefront",
  "label": "migrate",
  "kind": "task",
  "image": "postgres:16",
  "domain": "migrate.storefront.shop.fghj.internal",
  "environment": ["PGDATABASE=shop", "PGHOST=${FGHJ_SERVICE_FQDN:db}",
                  "PGPASSWORD=dev", "PGUSER=shop"],
  "command": ["psql", "-v", "ON_ERROR_STOP=1", "-f", "/schema.sql"],
  "volumes": [{ "host": "./schema.sql", "container": "/schema.sql", "read_only": true }],
  "restart": "no",
  "stop_grace_period": 10,
  "run_policy": "on_start",
  "migrates": "db.storefront"
}
```

The edges, trimmed to the parts that matter here:

```json
{ "from": "web.storefront",     "to": "db.storefront",      "kind": "depends-on", "condition": "service_healthy" }
{ "from": "migrate.storefront", "to": "db.storefront",      "kind": "depends-on", "condition": "service_healthy" }
{ "from": "web.storefront",     "to": "migrate.storefront", "kind": "depends-on", "condition": "service_completed_successfully", "implied": true }
```

The last one is the edge nobody wrote: `web` waits on `db`, so it waits on
`db`'s migration. Start order is a topological walk of them: `db` first,
wait for `pg_isready`, then `migrate`, wait for it to exit 0, then `web`.

`restart: "no"` is what a task gets; writing a `restart` on one is a
blocking warning, since a restart policy on a container whose purpose is to
exit would restart it forever.

A `healthcheck` on a task is refused too, rather than silently ignored.
An exited container can never report Docker-healthy, which is exactly the
hole this node kind fills; a task's completion predicate is its exit code.

## Run it

Press **Start** on `web`. `migrate` doesn't get a card of its own: it's
part of `db`, so it's drawn as a second, smaller segment of `db`'s status
bar. Watch it go `running` briefly and settle on **completed** — hover it
for what that means, click it for its logs and its own Start button.

Three states are reserved for tasks:

| State | Means |
|---|---|
| **completed** | exited 0. Success. |
| **failed** | exited non-zero. Everything downstream of it was not started. |
| **finishing** | exited, but fghj hasn't re-inspected it yet — no exit code read, so no verdict. Says "no answer", not "success". |

**failed** is a real gate, not a warning: a migration that exits non-zero
blocks everything waiting on `db` rather than letting it start against a
half-migrated database. Break your SQL on purpose to see it — `web` will
not come up.

## A seed

A seed fills the database with data to work against. It's a task too, but
it's the opposite of a migration in every way that matters: it should run
**once per database**, not on every start, and nothing should have to wait
for it. In `storefront/seed.sql`:

```sql
insert into orders (item, price) values ('mug', 1200), ('tote', 2500);
```

```yaml
  seed:
    image: postgres:16
    command: ["psql", "-v", "ON_ERROR_STOP=1", "-f", "/seed.sql"]
    seeds: [db]
    environment:
      PGHOST: ${FGHJ_SERVICE_FQDN:db}
      PGUSER: shop
      PGPASSWORD: dev
      PGDATABASE: shop
    volumes:
      - host: ./seed.sql
        container: /seed.sql
        read_only: true
```

`seeds: [db]` orders it after `db` is up and migrated. It runs **only when
`db`'s data is new**: fghj records the volume it last succeeded against, so
the seed is skipped on every later start, and runs again once you remove the
volume and a fresh one is made. That's why the seed above has no
`on conflict` — it never runs over data it already filled. Keep a seeded
store's data in a named volume (chapter 2 did); a bind mount has no creation
time fghj can see.

Nothing waits on a seed, so starting `web` doesn't run it. Starting
everything does (when the data is new), so does a flow that lists it —
[chapter 5](/tutorial/05-flows/) — and the Start button on its card always
runs it, data or not. A failed seed always runs again.

Seeds are hidden in the graph by default, since most starts skip them. The
**Seeds** button in the header shows them.

:::note[What about `run: once`?]
A plain task can also say `run: once`: at most once per run, keyed on the
task's own container. It is not re-run when your code changes, and it's
wrong for a seed — a container removed while the volume stays reseeds on top
of the data, a volume wiped while the container stays leaves an empty
database marked done. Use `seeds:` for data, `migrates:` for a schema.
:::

## Check it landed

```bash
fghj exec db.storefront -- psql -U shop -d shop -c '\d orders'
```

`fghj exec` takes a **node id** — the same `db.storefront` you've been
reading in the graph — not a container name. It's full duplex, so an interactive shell works
too:

```bash
fghj exec db.storefront -- psql -U shop -d shop
```

---

**Next:** [A second repo](/tutorial/04-a-second-repo/) — where the
federation actually starts.
