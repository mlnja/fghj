# Docker integration and background downloads

## Building without Compose

`fghj` never shells out to `docker compose`, even though it deliberately
*mimics* Compose in a few places for tooling compatibility — `run_container`
sets `com.docker.compose.project`/`.service`/`.oneoff` labels so Docker
Desktop's own UI, and `docker ps`/`compose ls`, group a run's containers
together as if they were a real Compose stack, even though `docker.rs`
talks to the Engine API directly via `bollard`.

A few choices in `src/docker.rs` exist specifically because `bollard`'s API
is lower-level than `docker build`/`docker run`:

- **`build_image`** has to hand the Docker daemon a tar stream of the build
  context itself — there's no "build from this directory" call at the API
  level. It builds that tar in a blocking task (`tar::Builder::append_dir_all`)
  before ever touching the async `docker.build_image` call.
- **`run_container`** has to construct the create/start sequence manually,
  and — because a failed `start_container` can leave a container behind in
  `Created` state — always best-effort force-removes on any error in that
  sequence, so a retried run never trips over a dangling half-created
  container blocking the same name.
- **Ports**: `RunOpts.ports` is a list of `(container_port, Option<host_port>)`
  — `None` means "publish to whatever ephemeral localhost port Docker
  picks", `Some(p)` binds exactly `127.0.0.1:p`. Every published port is
  bound to `127.0.0.1` specifically, not `0.0.0.0` — containers are never
  meant to be reachable from outside the host.
- **`inspect_status`** is the one place both "does this container exist"
  and "what host port did it get published on" are answered, and it
  deliberately returns `Ok(None)` rather than an error for a nonexistent
  container (404 from the Engine API) — every caller (the reconciler, the
  route-building pass in `start_node`, `ensure_running`'s liveness check)
  treats "doesn't exist" as ordinary control flow, not an error path.

## Two log-reading modes

`logs_tail` is one-shot: fetch the last N lines, return them as a string —
backs the Drawer's "load logs" button. `logs_follow` is continuous and never
terminates on its own (until the container stops or the caller drops the
stream) — backs the SSE live-follow endpoint
(`daemon::get_run_logs_stream`), which `Drawer.svelte` opens automatically
whenever the Logs tab is showing a running container. The comment on
`logs_follow` calls out a real subtlety: bollard's `Docker::logs` clones
what it needs out of `&self` up front, so the returned stream doesn't
borrow `docker` or `name` — it's safe to return from an axum handler after
both go out of scope, which is exactly what happens once the handler
function itself returns and only the `Sse` response stream lives on.

## Background download jobs: don't block the request

Cloning a repo can take anywhere from under a second to tens of seconds, and
a `pull-all` might clone several repos in sequence — far too slow for a
synchronous HTTP handler. `downloads::DownloadRegistry` runs each clone job
on its own OS thread (not a tokio task — `git clone`'s own process spawn and
blocking I/O don't need the async runtime) and lets the UI poll for
progress instead:

- **Keys**: `"node:<id>"` for a single-node download, `"pull-all"` for the
  whole-graph fixpoint pull, `"pull-flow:<flow>"` for a flow-scoped one
  (`pull_all_key`, shared between `start_pull_all` and the status-lookup
  handler so both agree on how a flow name becomes a key). A flow-scoped
  pull and the whole-graph one are tracked independently, so both can be
  polled — or even run — at once without one clobbering the other's
  status.
- **Idempotent starts**: `spawn` checks whether a job under this key is
  already `"running"` and, if so, just returns its current snapshot instead
  of starting a second concurrent clone of the same thing.
- **Log streaming**: `run_git_clone_logged` pipes the child's stdout *and*
  stderr (git's own progress meter writes to stderr, not stdout) into the
  same growing log string the UI polls, translating `\r` (a real terminal's
  "overwrite this line" convention for progress meters) into `\n` since
  that reads far better inside a `<pre>` block than a wall of
  carriage-returns.
- **Ordering**: the registry is backed by a `Vec`, not a map, specifically
  so iteration order reflects "job first started" rather than key sort
  order — `OperationsDrawer.svelte`'s job list relies on this to show jobs
  in a sensible, most-recent-first order (`DownloadRegistry::list` reverses
  the vec).

## `pull_all_logged`: a fixpoint, because cloning can reveal more repos

A single `resolve_universe` pass can only see stub nodes for dependencies
declared by repos *already on disk* — a not-yet-cloned repo's own
dependencies are, definitionally, unknown until it's cloned. `pull_all_logged`
therefore loops: resolve the graph, clone every currently-missing (and, if
`flow` is set, flow-reachable) service node, then resolve again — repeating
until a pass finds nothing left to clone. Each pass is guaranteed to make
progress (a cloned node always flips to `downloaded: true` on the next
resolve) or terminate, so the loop can't spin forever short of a
pathological clone that never actually lands the repo at its expected path.

## Privilege drop, shared with the resolver's branch-override path

`run_git_clone_logged` and `resolver::ensure_mirror` (used for the branch-
override build path — see [[run-lifecycle-and-registry]]) both take an
optional `&WorkspaceOwner` and apply it the same way (see
[[persistence-and-workspace-store]] for the full owner-capture story) and
both apply `store::harden_git_ssh` on top, as defense in depth against a
`git clone` subprocess hanging on an unanswerable interactive prompt.

## Status

Implemented: `src/docker.rs` (build/run/inspect/logs over `bollard`),
`src/downloads.rs` (`DownloadRegistry`, single-node/pull-all/pull-flow
jobs, log streaming). `daemon.rs` exposes all of it over HTTP (see
[[control-api-and-cli]]); `OperationsDrawer.svelte` and `Drawer.svelte` are
the two UI surfaces that poll it (see [[ui-architecture]]).
