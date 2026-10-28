---
title: 5. Flows
description: Repos you haven't cloned yet, shared infrastructure, flows as public start lists, and why selecting one highlights the graph instead of filtering it.
sidebar:
  order: 5
---

At `ch5` the app gets thumbnails. When a file finishes uploading, the api
puts a job on a Redis queue. A worker in its own repo, `files-worker`,
takes it off, renders a thumbnail from the original in S3, and tells the
api it's done. The Redis they share belongs to neither of them. It lives in
a fourth repo, `files-platform`.

```bash
git -C files-api checkout ch5
git -C files-web checkout ch5
```

Don't clone the other two. fghj will tell you about them first.

## Repos you don't have yet

`files-api` now includes the platform, and `files-web` includes the worker:

```yaml
# files-api/.fghj.yaml
include:
  platform: https://github.com/fghj-examples/files-platform.git
```

```yaml
# files-web/.fghj.yaml
include:
  api: https://github.com/fghj-examples/files-api.git
  worker: https://github.com/fghj-examples/files-worker.git
```

Neither is on disk. Resolve anyway:

```bash
fghj graph https://github.com/fghj-examples/files-web.git
```

Among the nodes are two you didn't declare:

```json
{
  "id": "files-worker",
  "label": "files-worker",
  "kind": "service",
  "repo": "https://github.com/fghj-examples/files-worker.git",
  "local_path": "files-worker",
  "domain": "files-worker.files.fghj.internal",
  "downloaded": false,
  "dirty": false,
  "flows": ["files-web/full"]
}
```

and the same for `files-platform`. This is the **fog of war**. fghj knows a
repo is there, because yours said so, and nothing else about it: not its
services, not its ports, not its own dependencies. A repo's contents aren't
knowable until it's on disk. `downloaded: false` is the UI's cue to draw it
as a placeholder with a download button.

`flows` is already filled in, because that part is known from *your* side:
`files-web/full` lists `worker/thumbnails`, a flow inside this repo. What
the flow holds is unknown until the repo is read.

The stub's id is the folder name the URL implies. Once the repo lands, the
stub is replaced by the nodes its file declares.

On the **Repos** tab, press **Download all**. The daemon clones what's
missing, re-resolves, and repeats until nothing new turns up. It's a loop
because each clone can reveal includes of its own: here the worker includes
the platform and the api too, which are already accounted for, so it stops
after one round. Then it pulls the images the services run as they are —
Postgres, MinIO, Redis, dbmate — each its own entry under **images**, so
starting the flow later only has to build. From a terminal, `fghj pull`
does the same.

The clones land on `main`, which is the finished example. Put them on this
chapter like the others:

```bash
git -C files-worker checkout ch5
git -C files-platform checkout ch5
```

## Shared infrastructure

`files-platform/.fghj.yaml` is one Redis and nothing else:

```yaml
services:
  redis:
    image: redis:7-alpine
    ports: ["6379"]
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      interval: 2
      retries: 15
    volumes:
      - name: redis-data
        container: /data

flows:
  cache: [redis]
```

Two repos need the same queue, and neither should own it. If `files-api`
declared the Redis, the worker would depend on the api's repo for
infrastructure that has nothing to do with the api. So it's in a repo of its
own, and both include it.

They include it *separately*, and that's the point of the
one-checkout-per-repo rule from chapter 4. `files-api` and `files-worker`
each name `files-platform`. There's still one folder on disk and one
`redis.files-platform` node, and both edges arrive at it.

## A flow is a public start list

The last key in that file is new:

```yaml
flows:
  cache: [redis]
```

A **flow** is a name and a list of what to start. Starting a flow starts
everything on its list and everything those can't start without: every
`depends_on` entry not marked `required: false`, followed all the way down,
across repos too. You never list the closure by hand.

Flows are how a repo publishes parts of itself. Chapter 4 said that across
repos, `depends_on` names a whole repo or a flow it publishes, never a
service. `files-platform` publishes `cache`, and the other repos wait on
that:

```yaml
# files-api/.fghj.yaml, under api:
    depends_on:
      db: {condition: service_healthy}
      s3: {condition: service_healthy}
      platform/cache: {}
```

`files-api` publishes three:

```yaml
flows:
  app: [api]
  storage: [s3]
  demo: [api, seed]
```

- **`app`** is the API and what it can't run without: its two stores, their
  migrations, and Redis.
- **`storage`** is just the object store and its migration, for a consumer
  that reads S3 and never calls the API.
- **`demo`** is `app` plus the seed. A seed is never started because
  something depends on it, so a flow that lists it is how it gets run.

`files-web` now waits on `api/app` rather than on all of `files-api`. And the
worker waits on exactly the parts it uses:

```yaml
# files-worker/.fghj.yaml
services:
  worker:
    build: .
    environment:
      REDIS_URL: redis://${FGHJ_SERVICE_FQDN:platform/redis}:6379
      S3_ENDPOINT: http://${FGHJ_SERVICE_FQDN:api/s3}:9000
      API_URL: http://${FGHJ_SERVICE_FQDN:api/api}:3000
      # …credentials
    depends_on:
      platform/cache: {}
      api/storage: {}
      api/app: {required: false}

flows:
  thumbnails: [worker]
```

Each fact is written once, by the repo that can know it. `files-api` knows
what its storage is. The worker knows that it reads storage. If `files-api`
later moves its objects behind a second service, it adds that to `storage`,
and the worker picks it up without changing a line.

Note the worker's `S3_ENDPOINT` is the **raw** zone, where the api's was the
http zone. The worker only ever talks to S3 itself and never hands a URL to
anyone, so it skips the proxy and the CA entirely. Same store, two callers,
and each one chose for its own reason.

## Needed sometimes

`api/app: {required: false}` is the worker saying "I call the api, but I
can run without it". It does call it, to mark a thumbnail done, but only at
the end of a job. If the api isn't there, the job goes back on the queue and
waits.

`required: false` means: started when a flow lists it, left alone otherwise,
and never waited on. It's drawn dashed in the graph. Start the worker's own
flow, `files-worker/thumbnails`, and you get the worker, S3 and Redis, and no
api. fghj says so before it starts, so it isn't a surprise:

```
'worker.files-worker' needs 'files-api/app' at runtime, but this run doesn't start it
```

## What each flow starts

The flow ids are named by the repo that declares them, so `files-web`'s
`browse` is `files-web/browse` and can't collide with anyone else's. The
frontend declares two:

```yaml
flows:
  # Log in, upload, download. No thumbnails: the jobs wait in Redis.
  browse: [web]
  # Everything, plus the sample files.
  full: [web, worker/thumbnails, api/demo]
```

The graph records which flows each node is in:

| Node | `files-web/browse` | `files-web/full` | `files-worker/thumbnails` |
|---|---|---|---|
| `web.files-web` | ✓ | ✓ | |
| `api.files-api` | ✓ | ✓ | |
| `db.files-api`, `migrate-db.files-api` | ✓ | ✓ | |
| `s3.files-api`, `migrate-s3.files-api` | ✓ | ✓ | ✓ |
| `redis.files-platform` | ✓ | ✓ | ✓ |
| `seed.files-api` | | ✓ | |
| `worker.files-worker` | | ✓ | ✓ |

A flow can list:

| Entry | Means |
|---|---|
| `web` | A service in this repo. |
| `browse` | Another flow in this repo. |
| `api/demo` | A flow `files-api` publishes. |
| `api` | All of `files-api`, and everything it includes. |

Never `api/api`: the same rule as `depends_on`, and the same blocking
warning, listing the flows the repo does publish. A flow can name flows that
name it back, even across repos, and each is expanded once.

## Highlight, not filter

In the UI, the flow picker is top-left. Select `files-web/full` and its nodes
and edges go accent-coloured.

Everything else stays on screen. This is deliberate, and it's the rule worth
remembering: **a flow highlights, it never hides.** What's on the graph is
driven by what's on disk, not by flow membership.

Hiding would make the tool lie to you. A container that's running, holding
a port and mounting a volume doesn't stop existing because you changed a
dropdown. And the node you most need to see is often the one you didn't
expect to be there. See
[Fog-of-war visibility](/concepts/fog-of-war-visibility/).

## Switch to it

Two buttons act on the selected flow:

- **Download flow**, on the Repos tab: clone every not-yet-downloaded repo
  this flow would start. On a large workspace that's the difference between
  fetching four repos and fetching forty.
- **Switch to flow**, on the Actual tab: make the environment this flow.
  Start its containers, recreating any whose config or code changed, and
  stop every other running container. Stop, not remove: their volumes and
  their state stay, and switching back starts them again.

With `files-web/full` selected, press **Switch to flow**. Redis and the
worker are created, and the api is recreated, because its code and its
environment changed since chapter 4. The ☰ next to the button
shows each step as it goes.

Then make some thumbnails. The samples were seeded in chapter 3, before
there was a queue, so they don't have any. The seed is in `full`, but it was
skipped: its stores' data isn't new. Its Start button always runs it, so
open `seed` (the **Seeds** button shows it) and press **Start**. It puts the
samples back and queues a job for each. The worker's logs show it working
through them:

```
files-worker waiting on files:thumbnail-jobs
thumbnail thumbs/….webp for samples/harbor.svg
thumbnail thumbs/….webp for samples/dunes.svg
thumbnail thumbs/….webp for samples/canopy.svg
skip samples/README.txt: text/plain is not an image
```

Reload `https://files.test` and the samples have pictures. Upload an image
and its thumbnail turns up a few seconds later.

Then select `files-web/browse` and switch to it. The worker is stopped. Upload another image: it uploads, and stays without a thumbnail,
because its job is sitting in Redis. Switch back to `full` and the worker
picks it up.

---

**Next:** [When it goes wrong](/tutorial/06-when-it-goes-wrong/): reading
the status vocabulary, and the places to look.
