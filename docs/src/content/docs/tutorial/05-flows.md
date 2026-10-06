---
title: 5. Flows
description: Naming a user journey, and why selecting one highlights the graph instead of filtering it.
sidebar:
  order: 5
---

Five nodes is already more than fits in your head at a glance, and a real
workspace has fifty. A **flow** is a name for one user journey through the
graph, and the list of dependencies that journey needs.

## Declare one

At the bottom of `storefront/.fghj.yaml`, as a new top-level key — a sibling
of `services:`, not nested inside it:

```yaml
flows:
  browse-and-buy:
    description: A shopper lands on the storefront, browses the catalog, and checks out.
    dependencies:
      - kind: service
        repo: https://github.com/you/catalog.git
```

`description` is required, and it's the point: the flow name is for the
picker, the description is for the colleague who has to work out whether
this is the journey they're debugging.

`dependencies` must have at least one entry. A flow with nothing in it
describes no journey, so the schema refuses it rather than letting you
create a label that means nothing.

The flow's dependency list uses the exact same `#Dependency` shapes a
service's does — `kind: service`, `kind: backing`, `kind: task`,
`kind: shared-backing`. Here it names the same catalog repo `web` already
depends on, which is the common case: the journey needs what the service
baseline needs. You get one node, now tagged with the flow — not a second
copy of it.

Where a flow earns its keep is the dependency that *isn't* in the baseline.
An end-to-end checkout journey might need a payments sandbox and a webhook
receiver that nobody needs for ordinary local work on the storefront. Put
those in the flow, and they're pulled and started when someone is working
that journey — and not otherwise.

## Any repo may declare one

There's no privileged repo. `catalog` could declare its own flow tomorrow,
rooted at its own service, without asking anyone. Whichever repo you hand to
`fghj graph` is the entry point; every flow found anywhere in the workspace
shows up in the picker.

If a repo declares more than one service, a flow has to say which of them
it's rooted at:

```yaml
flows:
  browse-and-buy:
    service: web
    description: …
```

Omit it and the single service is used. Omit it with two services declared
and you get a blocking warning naming your options:

```
'catalog' declares multiple services (admin, api); specify which one with `service:`
```

Note that the field is `service:` (one name — a flow has one root), while
the field on a `kind: service` *dependency* is `services:` (a list — one
block can want several services from the same repo). The warning names
whichever one applies to the block you're editing.

## What resolution adds

Resolve again, and every node that's part of the journey has picked up a
tag:

```
service   id=web.storefront         flows=['browse-and-buy']
backing   id=db.web.storefront      flows=['browse-and-buy']
task      id=migrate.web.storefront flows=['browse-and-buy']
service   id=api.catalog            flows=['browse-and-buy']
backing   id=cache.api.catalog      flows=['browse-and-buy']
```

Edges carry the same tag. Membership is computed by walking outward from the
flow's root service over `owns` and `depends-on` edges — so you never list
the transitive closure by hand. Declaring the catalog dependency dragged in
its Redis cache automatically, because `api.catalog` owns it.

Everything is in the flow here, which makes for a dull demo but a real
point: membership is derived, not declared. Add an admin tool to the
workspace that nothing in this journey depends on and it stays untagged.

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

- **Pull flow** — clone every not-yet-downloaded repo in this flow. On a
  large workspace this is the difference between fetching four repos and
  fetching forty.
- **Run flow** — ensure this flow's containers are running, and leave
  everything else untouched. Not "stop the others": fghj doesn't tear down
  what you didn't ask about.

So the flow is a *scope* for work, not a boundary in the environment. Both
buttons act on the same shared default run — see
[Run lifecycle & registry](/concepts/run-lifecycle-and-registry/).

---

**Next:** [When it goes wrong](/tutorial/06-when-it-goes-wrong/) — reading
the status vocabulary, and the three places to look.
