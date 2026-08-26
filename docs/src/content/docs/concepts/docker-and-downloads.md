---
title: Docker & downloads
description: Building and running containers via the Docker Engine API directly, and how background clone jobs keep the UI responsive.
---

## Building without Compose

`fghj` never shells out to `docker compose`, even though it deliberately
*mimics* Compose in a few places for tooling compatibility — every
container it runs carries Compose-style project/service labels, so Docker
Desktop's own UI (and `docker ps`/`compose ls`) groups a run's containers
together as if they were a real Compose stack, even though `fghjd` talks
to the Docker Engine API directly.

A few consequences of talking to the Engine API directly rather than
shelling out to `docker build`/`docker run`:

- **Building** requires handing the Docker daemon a tar stream of the
  build context itself — there's no "build from this directory" call at
  the API level.
- **Running** a container means constructing the create/start sequence
  manually, and — because a failed start can leave a container behind in
  a `Created` state — `fghjd` always best-effort force-removes on any
  error in that sequence, so a retried run never trips over a dangling
  half-created container blocking the same name.
- **Ports** published to the host are always bound to `127.0.0.1`
  specifically, never `0.0.0.0` — containers are never meant to be
  reachable from outside your machine.
- **Inspecting** a container's status deliberately returns "not found"
  rather than an error for a nonexistent container, so callers — the
  reconciler, route-building, a run's liveness check — can all treat
  "doesn't exist" as ordinary control flow, not an error path.

## Two log-reading modes

Fetching the last N lines of a container's logs backs the log drawer's
"load logs" button. A continuous, never-terminating log stream backs the
live-follow view — opened automatically over Server-Sent Events whenever
the logs panel is showing a running container. See
[UI architecture](/concepts/ui-architecture/).

## Background download jobs: don't block the request

Cloning a repo can take anywhere from under a second to tens of seconds,
and a "pull all" might clone several repos in sequence — far too slow for
a synchronous HTTP handler. Each clone job runs on its own OS thread and
streams progress into a growing log buffer the UI polls instead:

- **Idempotent starts**: kicking off a download that's already running
  under the same key just returns its current progress instead of
  starting a second concurrent clone of the same thing.
- **Independent tracking**: a single-node download, a whole-graph "pull
  all", and a flow-scoped pull are all tracked independently, so more than
  one can be running — or polled — at once without one clobbering
  another's status.
- **Log streaming**: both stdout and stderr are captured (Git's own
  progress meter writes to stderr, not stdout) into the same log, with
  carriage returns translated into newlines so a terminal progress meter
  reads sensibly inside the UI instead of as a wall of overwritten lines.
- **Ordering**: the job list shown in the UI is ordered "job first
  started," most-recent-first — not by any incidental key sort order.

## Pull all: a fixpoint, because cloning can reveal more repos

A single graph resolution can only see stub nodes for dependencies
declared by repos *already on disk* — a not-yet-cloned repo's own
dependencies are, by definition, unknown until it's cloned. "Pull all"
therefore loops: resolve the graph, clone every currently missing (and,
if a flow is selected, flow-reachable) service node, then resolve again —
repeating until a pass finds nothing left to clone. Each pass either makes
progress or terminates, so the loop can't spin forever short of a clone
that never actually lands the repo at its expected path.

## Privilege drop, shared with the branch-override build path

Both the download path and the branch-override build path (see
[Run lifecycle & registry](/concepts/run-lifecycle-and-registry/)) run
`git clone` as the real workspace owner rather than as root, using the
same identity-borrowing mechanism described in
[Persistence & workspace store](/concepts/persistence-and-workspace-store/),
as defense in depth against a clone subprocess hanging on an unanswerable
interactive prompt.
