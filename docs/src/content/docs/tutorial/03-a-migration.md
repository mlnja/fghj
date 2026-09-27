---
title: 3. A migration
description: The third node kind — one that is supposed to exit — plus ordering between siblings and why "exited" needs two readings.
sidebar:
  order: 3
---

The database is up but empty. A schema migration is not a service: it runs,
it finishes, and then it is *correctly* not running. That difference is why
fghj has a third node kind instead of a flag on the second.

## Why a kind and not a flag

For a service, `exited` is a problem. For a migration, `exited` is the
goal. Nothing downstream — the status badge, the reconciler, the "is this
node healthy" check — can tell those two apart without knowing which kind of
node it's looking at. A boolean on `#Service` would have left every consumer
to re-derive the distinction, and one of them would have got it wrong. See
[Terminating nodes](/concepts/terminating-nodes/).

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

Then a second entry in `web`'s `dependencies:`, after the `db` block:

```yaml
      - kind: task
        name: migrate
        image: postgres:16
        command: ["psql", "-v", "ON_ERROR_STOP=1", "-f", "/schema.sql"]
        after: ["db"]
        environment:
          PGHOST: ${FGHJ_SERVICE_FQDN:db}
          PGUSER: shop
          PGPASSWORD: dev
          PGDATABASE: shop
        volumes:
          - host: ./schema.sql
            container: /schema.sql
            read_only: true
```

Four fields carry the weight here.

**`command` is required.** A task *is* its command. A container with no
command would just run the image's own long-running `CMD`, which is a
service.

**`image` is optional, and giving one is the less common case.** Omit it and
the task runs *the owning service's own built image* — which is what a real
migration almost always wants (`rake db:migrate`, `alembic upgrade head`,
`npm run migrate`): your code, a different command, no second Dockerfile and
no second build. We give an image here only because our migration genuinely
isn't our code — it's stock `postgres:16` running `psql`.

**`after: ["db"]`** orders this task against a *sibling*, named the way that
sibling names itself. Without it, the only guaranteed ordering is "before
the owner", and a migration that beats Postgres to the socket fails.

Note what `after` cannot do: name a node in a different repo. Ordering
against an arbitrary node elsewhere in the workspace would be an edge
between two repos that never agreed to one — the exact coupling the flat
workspace model exists to prevent. `after` is scoped to siblings, and only
siblings.

**The bind mount** puts your `schema.sql` inside the stock image. `host:
./schema.sql` is relative, and a task has no checkout of its own, so it
resolves against the **owning service's** checkout root — the repo whose
`.fghj.yaml` declared it. Same rule Compose uses for paths relative to the
compose file.

## What the graph says

```json
{
  "id": "migrate.web.storefront",
  "label": "migrate",
  "kind": "task",
  "image": "postgres:16",
  "domain": "migrate.web.storefront.shop.fghj.internal",
  "environment": ["PGDATABASE=shop", "PGHOST=${FGHJ_SERVICE_FQDN:db}",
                  "PGPASSWORD=dev", "PGUSER=shop"],
  "command": ["psql", "-v", "ON_ERROR_STOP=1", "-f", "/schema.sql"],
  "volumes": [{ "host": "./schema.sql", "container": "/schema.sql", "read_only": true }],
  "restart": "no",
  "stop_grace_period": 10,
  "run_policy": "on_start"
}
```

Two edges, doing different jobs:

```json
{ "from": "web.storefront",        "to": "migrate.web.storefront", "kind": "owns" }
{ "from": "migrate.web.storefront", "to": "db.web.storefront",     "kind": "after" }
```

`owns` is why the task exists and what it's named after. `after` is purely
ordering. Start order is a topological walk of both: `db` first, wait for
`pg_isready`, then `migrate`, wait for it to exit 0, then `web`.

`restart: "no"` isn't something we wrote — it's *forced* for tasks. A
restart policy on a container whose purpose is to exit would restart it
forever.

A `healthcheck` on a task is an error rather than a silently ignored field.
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
fghj exec db.web.storefront -- psql -U shop -d shop -c '\d orders'
```

`fghj exec` takes a **node id** — the same `db.web.storefront` you've been
reading in the graph — not a container name. It targets the default run
unless you pass `--run`, and it's full duplex, so an interactive shell works
too:

```bash
fghj exec db.web.storefront -- psql -U shop -d shop
```

---

**Next:** [A second repo](/tutorial/04-a-second-repo/) — where the
federation actually starts.
