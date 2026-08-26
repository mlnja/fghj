---
title: Flat workspace model
description: No repo is root. Every repo is a peer, any repo can declare a flow, and resolution never blocks on an un-cloned dependency.
---

## No repo is special

There's no "root" or "platform" repo that owns the dependency graph. Every
repo is a peer, and any repo can declare a `flows:` block for a user
journey it cares about — an `auth-service` might declare a "simple login
flow", a `cart-service` might declare a "checkout flow", independently of
each other. The entry point is just whichever repo you happen to
`fghj wire` first — it could be the backend, it could be the frontend. The
only asymmetry that matters is dependency *direction* (a frontend has no
reference to its backend; the backend references the frontend), never
"importance."

All repos live as sibling folders in one flat **workspace** directory,
named by convention — the last path segment of the repo URL, with `.git`
stripped — though a dependency can override that via `local_path` if it
needs to.

## Lazy, partial resolution

Resolving the graph never blocks on a repo that isn't cloned yet. A
dependency whose local folder isn't present in the workspace renders as a
`downloaded: false` stub node instead of failing the whole resolve — you
can see it, see which flow it belongs to, and see that it isn't there yet.

**Pull all** clones every missing repo into the workspace by convention,
recursively, until nothing new appears — a fixpoint loop, since a
newly-cloned repo might declare its *own* flows or dependencies that
nothing could see before it existed on disk. See
[Docker & downloads](/concepts/docker-and-downloads/) for how that loop is
implemented.

## Where this shows up

- [Node identity & domains](/concepts/node-identity-and-domains/) — why a
  peer-repo model means node ids can't just be the author-declared name.
- [Fog-of-war visibility](/concepts/fog-of-war-visibility/) — why a
  pulled repo is always visible on the graph even if no flow reaches it.
- [Branch ownership model](/concepts/branch-ownership-model/) — why flow
  membership and live branch state are deliberately kept as separate
  concerns under this model.
