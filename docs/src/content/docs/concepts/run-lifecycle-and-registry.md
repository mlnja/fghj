---
title: Run lifecycle & registry
description: The workspace's one environment — how it's started, switched between flows, and kept honest against real Docker state.
---

## One environment, two verbs

A workspace has exactly **one** environment: one docker network, one
sidecar, one set of containers. There is nothing to create or name — the
first start creates it. Two verbs act on it:

- **Starting a named run** — "make exactly this run exist, from
  scratch." If a run with this id is already up, it's stopped and torn
  down first, then every non-flow-filtered node in the graph is started
  fresh. A named run is meant to be reproducible from a clean slate every
  time you hit start again.
- **Topping up the default environment** — "make sure everything reachable
  from this flow (or the whole graph) is running; never touch a container
  that's already alive." This backs both "Start default environment" and
  "Run flow" — both are just different scopes of the same idempotent
  top-up. `fghj` models **one** shared set of running containers per
  workspace, not a separate environment per flow, so picking a flow to run
  must never restart — or duplicate — whatever's already up because some
  other flow needed it too.

Liveness is checked directly against Docker on every call, never trusted
from persisted state alone — a container can be stopped or removed
out-of-band between calls, so only a fresh check is trustworthy.

## Branch overrides: removed

Every node always builds straight from the live workspace checkout, so
local edits are picked up on every run without needing a commit first.

This used to have a second mode — a run could override which branch a
specific node built from, via a throwaway mirror + checkout kept
separately from the live checkout. It was removed: see
[Branch ownership model](/concepts/branch-ownership-model/) for why (a
real permission bug in the mirror clone, plus no designed answer for an
override branch whose own config doesn't match the live graph's shape).

## The reconciler: read-only drift correction

A background loop periodically re-inspects every live run's containers
against real Docker state and writes back any status that changed —
including flagging a container that's vanished entirely (removed by hand,
outside `fghj`) as removed. This is explicitly analogous to a Kubernetes
controller's reconcile loop, but **read-only**: it never recreates,
restarts, or otherwise "heals" anything. If a container dies, the UI shows
that honestly on the next tick rather than `fghj` silently bringing it
back — you decide whether to restart it.

The same kind of check runs once at daemon startup, against whatever runs
were persisted from a previous `fghjd` lifetime — one container at a time,
not one run at a time. Containers Docker still knows about are restored
with a freshly inspected status; containers that have vanished are dropped
from the run; and only a run with nothing left alive is forgotten
entirely. Doing it per-run instead would mean one missing container
silently orphaned every other container in that run from `fghjd`'s
bookkeeping — they'd keep running, but nothing would route to them.

Only the *observed* half is refreshed. What fghj recorded that it wanted
the container to be doing survives the restart untouched, which is what
lets a container that died while `fghjd` was down come back reading as
drifted rather than as freshly correct.

## Which commit a container is running

Config drift covers more than `.fghj.yaml`. For a node fghj **builds**, the
checkout itself is an input: the same `build:` stanza at a different commit
produces a different container. So the hash that decides `synced` vs
`drifted` folds in the checkout's `HEAD` and whether its tree was dirty,
and a `git commit`, `git pull` or rebase moves a built node to **drifted**
without `.fghj.yaml` changing at all.

That matters because the image tag doesn't move. fghj tags what it builds
`fghj/<node>:<branch>` — stable across commits by design, so there's no tag
to notice. Before the checkout was part of the hash, you could pull and the
node stayed lit as `synced` while serving the old code.

Alongside the hash, each container records the branch and commit it was
actually built from. The hash is a digest, so on its own it can only report
*that* something moved; the recorded commit is what lets the UI say

```
a3f9c1 → 4f4cd9d
```

— built from one commit, checkout now on another. It's a snapshot taken when
the container was created and never refreshed: a field that followed the
checkout would always agree with it and could never show drift. The same two
facts are also stamped on the container as `fghj.source_branch` and
`fghj.source_head` labels, so `docker inspect` answers "what commit is this
running" without fghj in the loop.

Two caveats worth knowing:

- **`dirty` is one bit.** It catches the clean → dirty transition and
  nothing after it; a second edit to an already-dirty tree moves no field.
  `HEAD` is the input that moves on every commit, and it's exact.
- **Nothing acts on this.** Drift is reported, never healed — see above. A
  `git switch` changes the graph under a live environment constantly, and a
  background loop recreating containers in response would fight whoever is
  working in it. Reset the node when *you* want the new code.

A container started by an `fghjd` from before this was recorded reads back
with no source rather than a guessed one, and the UI says nothing instead of
claiming a commit fghj never saw.

## How the proxy finds a container

Each running container carries a list of routes — domain/host-port pairs
built from every port that's either `primary` or `name`d (see
[Node identity & domains](/concepts/node-identity-and-domains/)). This is
what lets the TLS proxy turn an incoming HTTPS request into a
`127.0.0.1:<port>` to relay to — `fghjd` runs on the host, outside the
Docker network, so it can't rely on Docker's own embedded per-network DNS
the way sibling containers can. See
[Local CA & TLS proxy](/concepts/local-ca-and-tls-proxy/). These routes are
persisted alongside the rest of a run's state, so routing survives a
`fghjd` restart along with everything else the startup reconciler
restores.

## Limitations

Route lookup only considers containers with a `"running"` status, which
excludes stopped containers correctly but can briefly still return a
route to a container that's been fully *removed* between reconcile ticks.
