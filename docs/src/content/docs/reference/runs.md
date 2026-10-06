---
title: Runs
description: The default run, what a second named run does and doesn't vary, and the three knobs that break isolation between them.
sidebar:
  hidden: true
---

Everything you do in fghj happens in the **default run**: the one shared
environment per workspace, the one you top up as you touch more of the
graph. It is the only run the UI will start for you, and for almost
everything it is the only one you want.

A second, *named* run can be stood up alongside it. This page is here
because the run id shows up in derived names whether or not you ever use
one — and because the feature is narrower than it looks.

## What a named run is for, and what it isn't

A named run varies exactly three things:

1. **Identity.** Its own network, container names, sidecar and
   run-qualified domains.
2. **State.** Every volume that isn't `scope: stable` is created fresh and
   empty for it.
3. **What's up.** Which of its nodes are running, stopped, reset or paused,
   independently of the default run.

It does **not** get its own copy of your source, and the config→run map has
no per-run parameters — `RunSpec` is just a run id and an optional flow. So
two runs standing up from one workspace are built from identical code with
identical configuration.

That makes it a **scratch environment**, not a review environment. It is
worth starting one when you want to point something destructive at a virgin
database — a migration you expect to fail, a load test, a seed script you're
still writing — and keep the environment you were working in intact. It is
not worth starting one to review a branch: both runs would build the same
bytes. See [Branch ownership model](/concepts/branch-ownership-model/) for
why the per-run branch pin that would have made that work was removed.

## Starting one

There is no button for it. The UI starts and stops the default run, and
lists any other run that exists so you can stop it. To create one, post to
the control API:

```bash
curl --unix-socket /var/run/fghjd.sock -X POST \
  -H 'content-type: application/json' \
  -d '{"run_id": "scratch"}' \
  'http://localhost/runs?workspace=shop'
```

`?workspace=` is required and is the wired workspace's id — `GET /workspaces`
over the same socket lists them.

The id is sanitized into a label — `Feature/JIRA-123` becomes
`feature-jira-123` — and an absent or empty id means the default run.
Once it exists it appears in the run list with its own **Stop**, and
[`fghj exec --run scratch <node>`](/cli/exec/) targets it.

## What gets its own copy

A named run is a full parallel environment:

- **Its own docker network.** `fghj-shop-scratch` alongside
  `fghj-shop-default`.
- **Its own containers**, one per node, with the run in the name:
  `fghj-shop-scratch-web-storefront` beside
  `fghj-shop-default-web-storefront`.
- **Its own sidecar**, so in-network DNS and TLS inside `scratch` sees only
  `scratch`'s routes.
- **Its own domains.** The run id folds into every name, right after the
  node id:

  ```
  web.storefront.scratch.shop.fghj.internal
  api.catalog.scratch.shop.fghj.internal
  db.web.storefront.scratch.shop.fghj.raw.internal
  ```

Open the first of those. It's a second storefront, in a second network,
talking to a second catalog — while the original keeps answering at its own
name, untouched.

## Why the FQDN templates exist

Look at what `DATABASE_URL` expands to inside `scratch`'s `web` container:

```
postgres://shop:dev@db.web.storefront.scratch.shop.fghj.raw.internal:5432/shop
```

Nobody wrote that string. If the config had hardcoded
`db.web.storefront.shop.fghj.raw.internal` — which is a perfectly correct
address, right up to the moment a second run exists — then `scratch`'s
storefront would be talking to the **default run's** database. It would
work, and it would be wrong, and the failure would show up as test data
appearing in the wrong environment.

That's the argument for `${FGHJ_SERVICE_FQDN:db}` in one sentence: a node's
address depends on which run it's in, so it can't be a literal in a file
that predates the run.

## One checkout, one branch — no matter how many runs

A named run does **not** get its own copy of your source. Every node builds
from the same workspace checkout the default run builds from. To stand a run
up on a different branch the sequence is: switch the branch in the
workspace, then start the run — which means the default run is now on that
branch too.

Which means two runs cannot be on two different branches of the same repo at
the same time. That's the same one-checkout rule from
[tutorial chapter 4](/tutorial/04-a-second-repo/#declare-the-dependency-before-the-repo-exists),
seen from the other side: branch identity lives on the one shared checkout
precisely so that two repos depending on a third can never demand two branches
of it at once. The conflict isn't resolved, it's made unrepresentable, and the
cost is real — a genuine need for two branches side-by-side has to be
serialised in wall-clock time.

**A per-run branch pin used to exist and was deliberately removed**, so it's
worth knowing you're not missing a flag. A run could once override which branch
a specific node built from, using a throwaway mirror clone kept separate from
the live checkout. Two things killed it: the mirror clone's permission model
was never sound under a privilege-dropping `fghjd` (the directory it clones
into is created root-owned before the owner is known), and there was no
designed answer for an override branch whose own `.fghj.yaml` describes a
*different graph* — a branch that renames a service or drops a flow leaves the
run's node set disagreeing with the workspace's. See
[Branch ownership model](/concepts/branch-ownership-model/) and
[Run lifecycle & registry](/concepts/run-lifecycle-and-registry/#branch-overrides-removed).

## Reaching into a specific run

Every CLI command that touches a run takes `--run`, defaulting to the shared
default:

```bash
fghj exec --run scratch db.web.storefront -- psql -U shop -d shop -c 'select count(*) from orders'
```

The node id is the same in both runs — ids are a property of the graph, not
of a run. Only the container name and the domain carry the run.

## Three knobs that break isolation, on purpose

Isolation between runs is the default, and every one of these opts out of
part of it. Each exists for a real reason, and each has a cost you should be
able to state before you use it.

### `domain_scope: stable`

On a service or a backing dependency:

```yaml
    domain_scope: stable
```

Drops the run id from the derived domain. The node answers at
`web.storefront.shop.fghj.internal` in *every* run. Reach for it when an
external system has one URL on file for you — an OAuth callback registered
with a third party, a webhook a sandbox posts to.

The cost: only one run can meaningfully own that name from the host at a
time, and fghj does not currently stop the second one from registering the
same route. Nothing rejects it and nothing warns; which run your browser
reaches is unspecified. Use it on the node that needs the fixed name, and
don't run two environments that both claim it.

### `host_port`

On a port:

```yaml
    ports:
      "1883":
        host_port: 1883
```

Pins the host-side published port instead of letting Docker pick an
ephemeral one. Needed for protocols whose clients hardcode a port and can't
be routed by name at all — raw MQTT, a custom TCP protocol, a database
client you can't configure.

The cost is the honest one: only one run can hold a given host port, so
starting the second run **fails to bind** rather than quietly getting its
own copy. A loud failure is the intended behaviour here.

### `scope: stable` on a volume

The tutorial uses this one for `pgdata`, where it is harmless — with a
second run it has teeth. A stable-scoped volume drops the run id from the
derived volume name, so `scratch`'s Postgres container mounts the **same
data directory** as the default run's Postgres container.

Two database engines on one data directory is how a data directory gets
corrupted. Postgres has defences and may refuse to start the second one, but
don't rely on being saved by them.

So pick one, per node:

- **You run one environment at a time** and want your test data to survive
  container recreation → `scope: stable`. Stop the default run before
  starting a second one.
- **You run environments in parallel** → `scope: run` (the default), and
  accept that each one starts with an empty database. This is what a
  [`kind: task`](/concepts/terminating-nodes/) migration is for: every fresh
  run gets its schema built automatically.

Nothing enforces this. Two concurrent runs both mounting one stable-scoped
volume is a configuration fghj will happily create for you — see
[Docker & downloads](/concepts/docker-and-downloads/) for where that is
recorded.
