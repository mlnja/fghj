---
title: 6. Two runs at once
description: A review run beside the default one — what gets its own copy, what doesn't, and the three knobs that break isolation on purpose.
sidebar:
  order: 6
---

Everything so far happened in the **default run**: the one shared
environment per workspace, the one you top up as you touch more of the
graph. Now start a second one beside it.

In the UI's **Actual** tab: **+ Review run**, type `pr-123`, **Start review
run**.

## What gets its own copy

A named run is a full parallel environment:

- **Its own docker network.** `fghj-shop-pr-123` alongside
  `fghj-shop-default`.
- **Its own containers**, one per node, with the run in the name:
  `fghj-shop-pr-123-web-storefront` beside
  `fghj-shop-default-web-storefront`.
- **Its own sidecar**, so in-network DNS and TLS inside `pr-123` sees only
  `pr-123`'s routes.
- **Its own domains.** The run id folds into every name, right after the
  node id:

  ```
  web.storefront.pr-123.shop.fghj.internal
  api.catalog.pr-123.shop.fghj.internal
  db.web.storefront.pr-123.shop.fghj.raw.internal
  ```

Open the first of those. It's a second storefront, in a second network,
talking to a second catalog — while the original keeps answering at its own
name, untouched.

## This is where the templates paid off

Look at what `DATABASE_URL` expands to inside `pr-123`'s `web` container:

```
postgres://shop:dev@db.web.storefront.pr-123.shop.fghj.raw.internal:5432/shop
```

Nobody wrote that string. If chapter 2 had hardcoded
`db.web.storefront.shop.fghj.raw.internal` — which is a perfectly correct
address, right up to the moment a second run exists — then `pr-123`'s
storefront would be talking to the **default run's** database. It would
work, and it would be wrong, and the failure would show up as test data
appearing in the wrong environment.

That's the argument for `${FGHJ_SERVICE_FQDN:db}` in one sentence: a node's
address depends on which run it's in, so it can't be a literal in a file
that predates the run.

## One checkout, one branch — no matter how many runs

A review run does **not** get its own copy of your source. Every node builds
from the same workspace checkout the default run builds from. Start a review
run to test a branch and the sequence is: switch the branch in the
workspace, then start the run.

Which means two runs cannot be on two different branches of the same repo at
the same time. That's the same one-checkout rule from
[chapter 4](/tutorial/04-a-second-repo/#declare-the-dependency-before-the-repo-exists),
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
fghj exec --run pr-123 db.web.storefront -- psql -U shop -d shop -c 'select count(*) from orders'
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

We already used this one, in chapter 2, for `pgdata` — and now it has teeth.
A stable-scoped volume drops the run id from the derived volume name, so
`pr-123`'s Postgres container mounts the **same data directory** as the
default run's Postgres container.

Two database engines on one data directory is how a data directory gets
corrupted. Postgres has defences and may refuse to start the second one, but
don't rely on being saved by them.

So pick one, per node:

- **You run one environment at a time** and want your test data to survive
  container recreation → `scope: stable`, as we did. Stop the default run
  before starting a review run.
- **You run review environments in parallel** → `scope: run` (the default),
  and accept that each one starts with an empty database. This is what the
  migration task in chapter 3 is for: every fresh run gets its schema built
  automatically.

For this tutorial, stop `pr-123` before doing anything real with it: the
**Stop** button on its row in the run list.

---

**Next:** [When it goes wrong](/tutorial/07-when-it-goes-wrong/) — reading
the status vocabulary.
