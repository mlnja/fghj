---
title: 5. Flows
description: Starting less than everything — flows as public start lists, and why selecting one highlights the graph instead of filtering it.
sidebar:
  order: 5
---

Five nodes is already more than fits in your head at a glance, and a real
workspace has fifty, across a dozen repos. Starting all of it works — it's
what production runs — but it's slow and heavy, and most days you're working
on one journey. A **flow** is a named list of what to start for one.

## Declare one

At the bottom of `storefront/.fghj.yaml`, as a new top-level key — a sibling
of `services:`, not nested inside it:

```yaml
flows:
  browse-and-buy: [web]
```

That's a whole flow: a name and a list. Starting it starts `web`, and
everything `web` can't start without — every `depends_on` entry not marked
`required: false`, followed all the way down, across repos too. You never
list the closure by hand:

```
service   id=web.storefront      flows=['storefront/browse-and-buy']
backing   id=db.storefront       flows=['storefront/browse-and-buy']
task      id=migrate.storefront  flows=['storefront/browse-and-buy']
service   id=api.catalog         flows=['storefront/browse-and-buy']
backing   id=cache.catalog       flows=['storefront/browse-and-buy']
```

The flow's id is `storefront/browse-and-buy`: flows are named by the repo
that declares them, so another repo's `browse-and-buy` is a different
flow.

Starting **no** flow starts everything — every service here and in every
repo included, all the way down. That's always correct, because it's
production. A flow is only ever a way to start less.

## Publish one

Everything is in the flow here, which makes for a dull demo — and the reason
is `depends_on: catalog`: `web` waits on all of `catalog`. Suppose `catalog`
grows an `admin` service with its own database, which `web` never talks to.
`storefront` can't trim it — it can't name `catalog`'s services. `catalog`
can. In `catalog/.fghj.yaml`:

```yaml
flows:
  browse: [api]
```

That says "for browsing, start `api`" (and so, through `api`'s own
`depends_on`, its cache). It's `catalog`'s statement about `catalog`, next
to the code it describes, on the same branch. Now `storefront` can wait on
just that:

```yaml
services:
  web:
    depends_on:
      # ...
      catalog/browse: {}
```

and `admin` stays stopped when you start `storefront/browse-and-buy`. Each
fact is written once, by the repo that can know it: `catalog` knows what
browsing needs from `catalog`; `storefront` knows that its journey browses.
If `catalog` later needs a search index for browsing, it adds it to
`browse`, and `storefront` picks it up without changing a line.

## What a flow can list

| Entry | Means |
|---|---|
| `web` | A service in this repo. |
| `checkout` | Another flow in this repo. |
| `catalog/browse` | A flow `catalog` publishes. |
| `catalog` | All of `catalog`, and everything it includes. |

Never `catalog/api` — the same rule as `depends_on`, and the same blocking
warning, listing the flows `catalog` does publish. A flow can name flows
that name it back, even across repos; each is expanded once.

A flow is for what a journey *uses sometimes*. A service that's only needed
on some paths — a search index for one page — is `required: false` in its
dependent's `depends_on`: started when a flow lists it, left alone
otherwise, and never waited on. It's drawn dashed in the graph.

If you start a flow that leaves out a `required: false` dependency of
something it starts, fghj says so before it starts:

```
'web.storefront' needs 'search.storefront' at runtime, but this run doesn't start it
```

## Highlight, not filter

In the UI, the flow picker is top-left. Select `browse-and-buy` and the
journey's nodes and edges go accent-coloured.

Everything else stays on screen. This is deliberate, and it's the rule worth
remembering: **a flow highlights, it never hides.** What's on the graph is
driven by what's on disk, not by flow membership.

The reason is that hiding would make the tool lie to you. A container that's
running, holding a port, and mounting a volume does not stop existing
because you changed a dropdown. And the node you most need to see is often
the one you didn't expect to be there. See
[Fog-of-war visibility](/concepts/fog-of-war-visibility/).

## What the flow is for, operationally

Two buttons in the header act on the selected flow rather than the whole
workspace:

- **Pull flow** — clone every not-yet-downloaded repo this flow would start. On a
  large workspace this is the difference between fetching four repos and
  fetching forty.
- **Switch to flow** — make the environment this flow: start its
  containers (recreating any whose config changed), and stop every other
  running container. Stop, not remove — their volumes and their state stay,
  and switching back starts them again.

Both act on the workspace's one environment — see
[Run lifecycle & registry](/concepts/run-lifecycle-and-registry/).

---

**Next:** [When it goes wrong](/tutorial/06-when-it-goes-wrong/) — reading
the status vocabulary, and the three places to look.
