---
title: Run lifecycle & registry
description: Default vs. named/review runs, and how fghj keeps run state honest against real Docker state.
---

## Two different verbs, one shared registry

`fghj` has two ways of starting containers, and picking the wrong mental
model for either would either destroy work unnecessarily or silently fail
to start what you asked for:

- **Starting a named/review run** — "make exactly this run exist, from
  scratch." If a run with this id is already up, it's stopped and torn
  down first, then every non-flow-filtered node in the graph is started
  fresh. A review run is meant to be reproducible from a clean slate every
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
were persisted from a previous `fghjd` lifetime: a run whose containers
are all still alive is restored with freshly inspected statuses; a run
missing even one container is dropped outright rather than being
presented as a run that's only partially there.

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
