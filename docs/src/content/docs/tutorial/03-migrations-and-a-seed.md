---
title: 3. Migrations and a seed
description: Tasks, services that are supposed to exit — a migration for each store, why "exited" needs two readings, and a seed that runs once per store's data.
sidebar:
  order: 3
---

Both stores are up and both are empty: no table, no bucket. A schema
migration isn't like `api` or `db`. It runs, it finishes, and then it is
*correctly* not running. fghj calls that a **task**.

```bash
git -C files-api checkout ch3
git -C files-api diff ch2 ch3
```

## Why fghj has to know

For a service, `exited` is a problem. For a migration, `exited` is the
goal. Nothing downstream (the status badge, the reconciler, the "is this
node healthy" check) can tell those two apart without knowing which kind of
node it's looking at. See [Terminating nodes](/concepts/terminating-nodes/).

A task is any service that's meant to exit: something waits for it to
**complete successfully**, or it says which **store** it writes to. Every
task in this chapter says the second thing.

## A migration for the database

The schema is a [dbmate](https://github.com/amacneil/dbmate) migration,
`files-api/db/migrations/20260101000000_create_files.sql`:

```sql
-- migrate:up
create table files (
  id           uuid primary key default gen_random_uuid(),
  -- null owner = a sample from the seed, visible to everyone
  owner_id     text,
  name         text        not null,
  content_type text        not null,
  size         bigint      not null,
  object_key   text        not null unique,
  thumb_key    text,
  status       text        not null default 'pending'
               check (status in ('pending', 'ready')),
  created_at   timestamptz not null default now()
);
```

and a service to run it:

```yaml
  migrate-db:
    image: ghcr.io/amacneil/dbmate:2
    command: ["--wait", "--no-dump-schema", "up"]
    migrates: db
    environment:
      DATABASE_URL: postgres://files:dev@${FGHJ_SERVICE_FQDN:db}:5432/files?sslmode=disable
      DBMATE_MIGRATIONS_DIR: /db/migrations
    volumes:
      - host: ./db
        container: /db
        read_only: true
```

Four things carry the weight here.

**`migrates: db`** is what makes `migrate-db` a task, and it does two more
things. `migrate-db` waits on `db` (healthy, since `db` has a healthcheck)
without a `depends_on` of its own. And everything that waits on `db` now
waits on `migrate-db` too, without saying so: `api` can't start against an
old schema because someone forgot a line. One migration per store, so the
order of two can't be a guess.

**`command` is required.** A task *is* its command. A container with no
command would just run the image's own long-running `CMD`, which is a
service.

**It runs on every start of `db`.** So the command has to be idempotent.
dbmate records what it has applied in a table of its own and skips it next
time, as every real migration runner does (Flyway, Alembic, Rails,
Prisma).

**The bind mount** puts this repo's `db/` directory inside the stock image.
`host: ./db` resolves against the repo's checkout root, as every relative
path in this file does, which is the same rule Compose uses for paths
relative to the compose file. Edit a migration and the next start sees it,
with no image to rebuild.

## A migration for the object store

A bucket is an object store's schema: nothing works until it exists.
`src/migrate-s3.ts` creates it if it's missing:

```ts
try {
  await s3.send(new HeadBucketCommand({ Bucket: bucket }));
  console.log(`bucket ${bucket} already exists`);
} catch {
  await s3.send(new CreateBucketCommand({ Bucket: bucket }));
  console.log(`created bucket ${bucket}`);
}
```

This one *is* our code, so it's built from this repo, the same as `api`,
with another command:

```yaml
  migrate-s3:
    build: .
    command: ["node", "src/migrate-s3.ts"]
    migrates: s3
    environment: *env
```

`build: .` here doesn't mean a second image to maintain. It's the same
Dockerfile and the same context, so it's the same image. Most real
migrations look like this (`rake db:migrate`, `alembic upgrade head`, `npm
run migrate`), and `migrate-db` using a stock image is the less common case.

`environment: *env` is plain YAML, not an fghj feature. The diff gives
`api`'s environment an anchor, `environment: &env`, and this reuses it, so
the task gets the same S3 endpoint, credentials and CA trust as the
service, from one place.

## What the graph says

```bash
fghj graph https://github.com/fghj-examples/files-api.git
```

```json
{
  "id": "migrate-db.files-api",
  "kind": "task",
  "image": "ghcr.io/amacneil/dbmate:2",
  "command": ["--wait", "--no-dump-schema", "up"],
  "volumes": [{ "host": "./db", "container": "/db", "read_only": true }],
  "restart": "no",
  "run_policy": "on_start",
  "migrates": "db.files-api"
}
```

The edges, trimmed to the parts that matter:

```
api.files-api        → db.files-api          service_healthy
api.files-api        → s3.files-api          service_healthy
migrate-db.files-api → db.files-api          service_healthy
migrate-s3.files-api → s3.files-api          service_healthy
api.files-api        → migrate-db.files-api  service_completed_successfully  implied
api.files-api        → migrate-s3.files-api  service_completed_successfully  implied
```

The two marked `implied` are the edges nobody wrote: `api` waits on `db`,
so it waits on `db`'s migration, and the same for `s3`. Start order is a
topological walk of them. Each store comes up and turns healthy, then its
migration runs and exits 0, and only then does `api` start. The two stores
and their migrations don't wait on each other, so they go side by side.

`restart: "no"` is what a task gets. Writing a `restart` on one is a
blocking warning, since a restart policy on a container whose purpose is to
exit would restart it forever.

A `healthcheck` on a task is refused too, rather than silently ignored.
An exited container can never report Docker-healthy, which is exactly the
hole this node kind fills: a task's completion predicate is its exit code.

## Run it

Press **Start** on `api`. Migrations don't get cards of their own. Each is
part of the store it migrates, so it's drawn as a second, smaller segment
of that store's status bar. Watch them go `running` briefly and settle on
**completed**. Hover a segment for what that means, and click it for its
logs and its own Start button.

Three states are reserved for tasks:

| State | Means |
|---|---|
| **completed** | Exited 0. Success. |
| **failed** | Exited non-zero. Everything downstream of it was not started. |
| **finishing** | Exited, but fghj hasn't re-inspected it yet: no exit code read, so no verdict. It says "no answer", not "success". |

**failed** is a real gate, not a warning. A migration that exits non-zero
blocks everything waiting on its store rather than letting it start against
a half-migrated database. Break the SQL on purpose to see it, and `api`
won't come up.

`/api/status` now has a bucket to report:

```json
{
  "db": "PostgreSQL 16.… on aarch64-unknown-linux-musl, …",
  "s3": { "endpoint": "https://s3.files-api.files.fghj.internal", "buckets": ["files"] }
}
```

## A seed

A seed fills the stores with data to work against. It's a task too, but
it's the opposite of a migration in every way that matters: it should run
**once per store's data**, not on every start, and nothing should have to
wait for it.

`src/seed.ts` uploads a few sample files to S3 and inserts the rows that
point at them:

```ts
for (const sample of samples) {
  const key = `samples/${sample.name}`;
  await s3.send(new PutObjectCommand({ Bucket: bucket, Key: key, Body: sample.body, … }));
  await db.query(
    `insert into files (owner_id, name, content_type, size, object_key, status)
     values (null, $1, $2, $3, $4, 'ready')`,
    [sample.name, sample.contentType, Buffer.byteLength(sample.body), key],
  );
}
```

```yaml
  seed:
    build: .
    command: ["node", "src/seed.ts"]
    seeds: [db, s3]
    environment: *env
```

`seeds: [db, s3]` lists both stores, because the rows point at the objects
and neither makes sense without the other. It orders the seed after both
stores are up and migrated. And it runs **only when a store's data is
new**: fghj records the volumes it last succeeded against, so the seed is
skipped on every later start and runs again once you delete either volume
and a fresh one is made. Keep a seeded store's data in a named volume, as
chapter 2 did. A bind mount has no creation time fghj can see.

Nothing waits on a seed, so starting `api` doesn't run it. Starting
everything does (when the data is new), and so does a flow that lists it,
in [chapter 5](/tutorial/05-flows/). The Start button on the seed's card
always runs it, data or not, and a failed seed always runs again.

Seeds are hidden in the graph by default, since most starts skip them. The
**Seeds** button in the header shows them. Press it, open `seed`, and press
**Start**.

:::note[What about `run: once`?]
A plain task can also say `run: once`: at most once, keyed on the task's own
container. It isn't re-run when your code changes, and it's wrong for a
seed. A container removed while the volume stays reseeds on top of the
data, and a volume wiped while the container stays leaves an empty database
marked done. Use `seeds:` for data and `migrates:` for a schema.
:::

## Check it landed

```bash
fghj exec db.files-api -- psql -U files -d files -c 'select name, status from files'
```

```
    name     | status
-------------+--------
 harbor.svg  | ready
 dunes.svg   | ready
 canopy.svg  | ready
 README.txt  | ready
(4 rows)
```

`fghj exec` takes a **node id**, the same `db.files-api` you've been
reading in the graph, not a container name. It's full duplex, so an
interactive shell works too:

```bash
fghj exec db.files-api -- psql -U files -d files
```

---

**Next:** [A second repo](/tutorial/04-a-second-repo/): where the
federation actually starts.
