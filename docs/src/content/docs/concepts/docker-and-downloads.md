---
title: Docker & downloads
description: Running containers via the Docker Engine API directly, building with your own docker buildx, and how background clone jobs keep the UI responsive.
---

## Building without Compose

`fghj` never shells out to `docker compose`, even though it deliberately
*mimics* Compose in a few places for tooling compatibility — every
container it runs carries Compose-style project/service labels, so Docker
Desktop's own UI (and `docker ps`/`compose ls`) groups a run's containers
together as if they were a real Compose stack, even though `fghjd` talks
to the Docker Engine API directly.

Builds are the one exception: they go through *your* `docker buildx build`,
so they run on the builder, the cache and the registry logins your own
`docker build` uses (see [Which BuildKit builds it](#which-buildkit-builds-it)).
`fghj` still makes the build context itself, a tar stream, and hands it to the
CLI on stdin.

A few consequences of talking to the Engine API directly for everything else:

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
uploaded on **every** build, so anything large that isn't excluded shows up
here as a cost paid on every start.

`.dockerignore` is honoured, with the same root-anchored patterns the Docker
CLI uses — a bare `node_modules` excludes the top-level one, and `**/node_modules`
is how you reach nested copies. If this number looks far bigger than your
source, something large is reaching the builder that probably shouldn't:
`.git` is the common culprit, and it does more than cost time — Go stamps VCS
information into a binary whenever it finds a repository beside the source,
and `git` inside the builder cannot read it, which fails the build outright.

On failure, the end of the build's own output: the last 40 lines, which
hold the failing step, its compiler errors and BuildKit's summary:

```
#9 [3/3] RUN go build -o /out/app ./cmd/app
#9 0.912 cmd/app/main.go:12:2: undefined: handler
#9 ERROR: process "/bin/sh -c go build -o /out/app ./cmd/app" did not complete successfully: exit code: 1
```

Only that tail is kept, in the event. Nothing is shown while a build is still
running.

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

So `fghjd` builds with **your own `docker buildx build`**, run as the
workspace owner rather than as root. Whatever builder your `docker build`
would pick, `fghj`'s build picks too, along with:

- **Your registry logins.** A `FROM` on a private registry (ECR, GHCR, a
  company mirror) uses your credential helper or keychain, just as on the
  command line. On macOS the CLI runs inside your login session, because
  that's the only place the keychain answers.
- **Your build cache.** A repo you just built on the command line is warm
  for `fghj` too.

The builder is named in the start event so you never have to guess which one
ran:

```
building image   fghj/shop-storefront:main  ·  /Users/me/src/storefront  ·  buildkit: orb-builder (docker buildx)
```

#### When there's no `docker` CLI

If `fghjd` can't find a `docker` binary with the buildx plugin, it builds
through the Docker API instead. A CLI build that *fails* is not retried this
way. The fallback still follows your selected buildx builder, as far as an
API client can:

- **It runs its own BuildKit container** (`fghj_buildkit`, with a
  `fghj_buildkit_state` cache volume) on the **same image** as buildx's
  builder, pinned by image id. It never pulls that image: buildx's container
  already has it on your machine, and a pull would need registry logins the
  daemon doesn't have. This is a second build cache, so the first build of a
  repo is cold.
- **Registry logins don't reach it.** A private `FROM` fails there.
- **The image crosses the socket twice.** A BuildKit in a container can't
  write to `dockerd`'s image store, so the build exports a tarball and `fghj`
  streams it back in.
- **A failure shows only the failing step and its exit code.** The step's
  own output isn't available on this path.

The start event reads `buildkit: orb-builder` (no `docker buildx`) on this
path, or `buildkit: dockerd` when no container-backed builder is selected.

## Volumes: two shapes, one Docker primitive

Every container starts with one bind `fghj` adds itself, ahead of anything
declared: `/var/lib/fghjd/certs` read-only at `/etc/fghj/certs`, holding the
local CA's cert and a full trust bundle. See [Local CA & TLS proxy → Trust
files for containers](/concepts/local-ca-and-tls-proxy/#trust-files-for-containers).
Being first in the list means a declaration at that same container path wins,
the same way it would against anything else.

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
declared it**.

The node-id qualification is the part worth understanding, because it used
to be absent. Keying only on the author-chosen `name` meant two unrelated
repos that both happened to call a volume `data` silently landed on one
Docker volume — two Postgres containers on one data directory, with no
error anywhere, because Docker will happily mount the same volume twice.
The qualification makes accidental sharing impossible; deliberate sharing
is then spelled out with `shared: true` on **both** declarations, which
drops the qualification so the label alone decides identity. Reach for it
rarely: the usual reason to want it — several services behind one database
— is already one `services:` entry that they all `depends_on`, which gives you one *node*, and
therefore one container and one volume, with no name coincidence carrying
any weight.

Stopping a node or switching flows never deletes a volume, which is what
lets its data survive a restart. Only removing the workspace does: that
discards its containers, network and volumes together.

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
dependencies are, by definition, unknown until it's cloned. "Download all"
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
