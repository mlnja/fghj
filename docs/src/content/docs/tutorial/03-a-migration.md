---
title: 3. A migration
description: A task — a service that is supposed to exit — what makes something one, and why "exited" needs two readings.
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

You don't mark a task as one. You say what Compose says: that `web` waits
for it to **complete successfully**. That's the only reading of "wait for
this" that makes sense for something meant to exit, so it's what makes it
a task.

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

Then a third service, and one more line in `web`'s `depends_on`:

```yaml
services:
  web:
    # ...as before
    depends_on:
      db: {condition: service_healthy}
      migrate: {condition: service_completed_successfully}

  migrate:
    image: postgres:16
    command: ["psql", "-v", "ON_ERROR_STOP=1", "-f", "/schema.sql"]
    depends_on:
      db: {condition: service_healthy}
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

**`service_completed_successfully`** is what makes `migrate` a task. Once
something waits on it that way, everything that waits on it has to: a
second service waiting on `migrate` with plain `depends_on: [migrate]`
would be expecting it to stay up, and fghj refuses that start rather than
guess which one you meant.

**`command` is required.** A task *is* its command. A container with no
command would just run the image's own long-running `CMD`, which is a
service.

**`image` here is the less common case.** A real migration is usually your
own code with another command (`rake db:migrate`, `alembic upgrade head`,
`npm run migrate`), so it would say `build: .` — the same build as `web`,
no second Dockerfile. We use an image only because our migration genuinely
isn't our code — it's stock `postgres:16` running `psql`.

**`migrate`'s own `depends_on: db`** orders it after Postgres is healthy. A
migration that beats Postgres to the socket fails.

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
  "run_policy": "on_start"
}
```

The edges, trimmed to the parts that matter here:

```json
{ "from": "web.storefront",     "to": "db.storefront",      "kind": "depends-on", "condition": "service_healthy" }
{ "from": "web.storefront",     "to": "migrate.storefront", "kind": "depends-on", "condition": "service_completed_successfully" }
{ "from": "migrate.storefront", "to": "db.storefront",      "kind": "depends-on", "condition": "service_healthy" }
```

Start order is a topological walk of them: `db` first, wait for
`pg_isready`, then `migrate`, wait for it to exit 0, then `web`.

`restart: "no"` is what a task gets; writing a `restart` on one is a
blocking warning, since a restart policy on a container whose purpose is to
exit would restart it forever.

A `healthcheck` on a task is refused too, rather than silently ignored.
An exited container can never report Docker-healthy, which is exactly the
hole this node kind fills; a task's completion predicate is its exit code.

## Run it

**Start default environment.** In the UI, `migrate` appears, goes
`running` briefly, and settles on **completed** — with a tooltip that says
what that means:

> task finished successfully — it is meant to exit, not stay up

Three states are reserved for tasks:

| State | Means |
|---|---|
| **completed** | exited 0. Success. |
| **failed** | exited non-zero. Everything downstream of it was not started. |
| **finishing** | exited, but fghj hasn't re-inspected it yet — no exit code read, so no verdict. Says "no answer", not "success". |

**failed** is a real gate, not a warning: a task that exits non-zero blocks
its dependents rather than letting them start against a half-migrated
database. Break your SQL on purpose to see it — `web` will not come up.

## Runs every time, on purpose

`run_policy: "on_start"` in the graph output is the default, and it means
this task runs again on **every** start and every top-up. Which is why
`create table if not exists` and `ON_ERROR_STOP=1` are both there: the
command has to be idempotent, because it will be re-run.

The alternative is `run: once` in the config — at most once per run, for a
task that's expensive or destructive to repeat. Be clear about its cost
before reaching for it: *at most once per run, full stop*. It is not re-run
when your code changes. A `git pull` that adds a migration will not cause a
`once` task to run again, even though fghj can see the commit moved. That's
why the default is the other way round.

One useful exception: fghj's evidence that a `once` task is done is the
container itself. If that container is removed out of band, the task runs
again rather than being assumed complete.

## Check it landed

```bash
fghj exec db.storefront -- psql -U shop -d shop -c '\d orders'
```

`fghj exec` takes a **node id** — the same `db.storefront` you've been
reading in the graph — not a container name. It targets the default run
unless you pass `--run`, and it's full duplex, so an interactive shell works
too:

```bash
fghj exec db.storefront -- psql -U shop -d shop
```

---

**Next:** [A second repo](/tutorial/04-a-second-repo/) — where the
federation actually starts.
