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

### What a build tells you, and what it can't

A node's **Events** tab narrates each build as three lines. When it starts,
the tag alongside the inputs it resolved to — context directory, and
`-f`/`--target`/platform whenever they differ from the default:

```
building image   fghj/shop-storefront:main  ·  /Users/me/src/storefront  ·  --target dev  ·  buildkit: orb-builder
```

The context directory is there because a `context:` that resolved somewhere
unexpected looks identical in the events pane to one that didn't — the tag
is derived from the node id, so it's the same either way. The `buildkit:`
field names the engine, which matters more than it sounds like it should:
see [Which BuildKit builds it](#which-buildkit-builds-it).

On success, how long it took and how much context was shipped:

```
building image   412.0 MiB context in 1m 12s
```

The size is the number nothing else would surface. The context is tarred and
uploaded whole on **every** build, so a `node_modules` or a `.git` that
belongs in `.dockerignore` shows up here as a cost paid on every start.

On failure, the failing step and its exit code, pulled out of BuildKit's
error:

```
failing step: /bin/sh -c go build -o /out/app ./cmd/app
exit code: 1
```

**What is not there is the step's own output** — the compiler errors. BuildKit
streams those over a separate gRPC channel that the Docker API client `fghjd`
uses doesn't expose, so the daemon never sees them. The event says so and
gives you the command to run by hand:

```
docker build -f Dockerfile /Users/me/src/storefront
```

That's a real gap rather than a design choice — and a shallow one. BuildKit
hands back a reference specifically so a caller can subscribe to the build's
status stream, log lines included; `bollard` just keeps the channel it would
need crate-private. Until that opens up, a build failure is the one thing that
will send you to a terminal.

### Which BuildKit builds it

`docker build` on your machine does not necessarily use the BuildKit inside
`dockerd`. If `buildx` has a container-backed builder selected — OrbStack and
Docker Desktop both set one up, and `docker buildx ls` marks it with a `*` —
then the CLI sends your build to *that* container, which runs its own BuildKit
release on its own schedule. The one embedded in `dockerd` is a different
engine at a different version.

That produced a genuinely maddening class of bug: a Dockerfile that `docker
build` compiles happily and `fghj` refuses, with no difference anywhere in
the repo. On the machine this was first diagnosed on, the gap was three minor
versions — buildx's builder on BuildKit v0.32.2, `dockerd`'s embedded one on
v0.29.0 — which is enough for the two to disagree about whether a `RUN` may
write to a directory that doesn't exist yet.

So `fghjd` reads `~/.docker/buildx/current` and `instances/<name>` — the
**workspace owner's** home, not the daemon's, since `fghjd` runs as root and
`/var/root/.docker` holds no buildx config at all — and when a
container-backed builder is selected it builds on BuildKit at the **same
image digest** as that builder. The engine is named in the start event so you
never have to guess which one ran:

```
building image   fghj/shop-storefront:main  ·  /Users/me/src/storefront  ·  buildkit: orb-builder
```

It reads `buildkit: dockerd` when no container-backed builder is selected, and
that path is unchanged from before.

Two things worth knowing about how this is done:

- **`fghj` runs its own BuildKit container** (`fghj_buildkit`, with a
  `fghj_buildkit_state` cache volume) rather than reusing buildx's. It is
  pinned to the same image digest, so it is the same engine, but it is a
  second container and a second build cache — the first `fghj` build of a repo
  is cold even if you built it on the CLI a minute earlier. This is
  deliberate: stopping or reconfiguring the builder your own `docker build`
  depends on is not `fghj`'s to do, and `bollard` enforces that by refusing to
  adopt containers it didn't create. When the digest moves, `fghj` replaces its
  container and keeps the cache volume.
- **The image crosses the socket twice.** A BuildKit running in a container
  has no access to `dockerd`'s image store — this is why `buildx` itself needs
  `--load` — so the build exports a docker-format tarball and `fghj` streams
  it back into `/images/load`. Compared with the embedded builder, that is one
  extra copy of every image you build.

## Volumes: two shapes, one Docker primitive

A volume declaration is either a bind mount or a named volume, never both
— see [.fghj.yaml reference: Volumes](/reference/fghj-yaml/#volumes) for how
to write each. Both shapes end up in the exact same place at the Docker
layer: a bind mount is `"host/path:container/path"`, a named volume is
`"volume-name:container/path"` — Docker itself tells them apart by
whether the left side contains a `/`, so both are just formatted strings
in the same list handed to the container create call.

A bind mount's host path, if relative, resolves against the live workspace
checkout `fghjd` actually built the image from. It is
**not** sandboxed to that repo: an absolute path, or one that walks up
with `..`, passes straight through to Docker unchanged. That's a
deliberate choice, not an oversight — it's what lets a service bind-mount
a sibling repo's checkout directly, the same way Docker Compose would.

A named volume's real Docker name is *derived*, never the literal string
you write — the same principle as a node's `*.fghj.internal` domain (see
[Node identity & domains](/concepts/node-identity-and-domains/)). It's
built from the volume's own `name`, **qualified by the id of the node that
declared it**, and folding in the run id unless `scope` is `"stable"`.

The node-id qualification is the part worth understanding, because it used
to be absent. Keying only on the author-chosen `name` meant two unrelated
repos that both happened to call a volume `data` silently landed on one
Docker volume — two Postgres containers on one data directory, with no
error anywhere, because Docker will happily mount the same volume twice.
The qualification makes accidental sharing impossible; deliberate sharing
is then spelled out with `shared: true` on **both** declarations, which
drops the qualification so the label alone decides identity. Reach for it
rarely: the usual reason to want it — several services behind one database
— is already `kind: shared-backing`, which gives you one *node*, and
therefore one container and one volume, with no name coincidence carrying
any weight.

One hazard the qualification doesn't close: `scope: "stable"` means one
volume across every run *including two runs that are up at the same time*,
so a default run's database and a named run's database can still end up on
one data directory. It's documented where an author meets it — the
[volume table](/reference/fghj-yaml/#setting-a-named-volume) and
[Runs](/reference/runs/) — and not enforced anywhere
yet.

Volumes are never deleted by `fghj` — stopping a run tears down its
containers and network only, which is what lets a volume survive a
restart in the first place. A `"run"`-scoped named run that's stopped
and never restarted leaves its volume behind, with no cleanup command yet.

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

## Privilege drop for clones

The download path's own `git clone` runs as the real workspace owner
rather than as root, using the identity-borrowing mechanism described in
[Persistence & workspace store](/concepts/persistence-and-workspace-store/),
as defense in depth against a clone subprocess hanging on an unanswerable
interactive prompt. (The now-removed branch-override build path — see
[Run lifecycle & registry](/concepts/run-lifecycle-and-registry/) — used
to share this same privilege drop for its own mirror clone; that code no
longer exists.)
