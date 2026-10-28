# Docker integration and background downloads

## Which Docker fghjd talks to

The same Docker the user's own `docker` command talks to, and fghjd is told
which one rather than guessing.

`/var/run/docker.sock` is a symlink owned by whichever engine set it last,
and it can point at nothing: a Docker Desktop uninstall leaves its
privileged helper behind, which repoints the link at Docker Desktop's
socket on the next boot. The user's `docker` doesn't care, because it
follows their `DOCKER_HOST`, `DOCKER_CONTEXT` or current context. Root
`fghjd` has none of those: they live in the user's shell and `~/.docker`.

So the CLI works it out, as the user (`docker::users_docker_host`), in
Docker's own order: `DOCKER_HOST`, then `DOCKER_CONTEXT`, then
`currentContext` from `$DOCKER_CONFIG` or `~/.docker/config.json` (the
endpoint read from `contexts/meta/<sha256 of the name>/meta.json`, not from
`docker context inspect`, so a missing `docker` binary doesn't matter), then
`/var/run/docker.sock`. Only `unix://` sockets are accepted. `fghj wire`
sends it to `fghjd` (`POST /daemon/docker`), which saves it in
`daemon-state.json` and dials it on every start. `fghj doctor --fix` does the
same without wiring anything.

Before anything is wired fghjd dials `/var/run/docker.sock` if it's live,
and otherwise has no Docker; the doctor reports that as pending, not as a
failure, because nothing can run until something is wired anyway.
`DOCKER_HOST` in fghjd's own environment overrides all of it.

An earlier version guessed instead: as root, it read the `~/.docker` of
whoever owned `/dev/console`. That needed a different guess per OS
(`/dev/console` is root's on Linux, so logind there), broke with two users
logged in over ssh, and could never see an environment variable. Being
told by the CLI has none of those problems.

The client is made once and shared by every workspace, so it can't be
swapped in place. When `wire` names a different socket from the one this
process dialled, fghjd answers, then shuts down cleanly, and its service
manager (launchd, systemd) starts it again on the new socket. `wire` waits
until `/daemon/status` reports the new socket before carrying on. Run in the
foreground, `fghjd` just exits, and `wire` says to start it again.

Two users on one machine with different engines can't both be served by one
fghjd: the last to wire wins.

## Building without Compose

`fghj` never shells out to `docker compose`, even though it deliberately
*mimics* Compose in a few places for tooling compatibility — `run_container`
sets `com.docker.compose.project`/`.service`/`.oneoff` labels so Docker
Desktop's own UI, and `docker ps`/`compose ls`, group a run's containers
together as if they were a real Compose stack, even though `docker/`
talks to the Engine API directly via `bollard`.

A few choices in `src/docker/` exist specifically because `bollard`'s API
is lower-level than `docker build`/`docker run`:

- **`build_image`** has to hand the Docker daemon a tar stream of the build
  context itself — there's no "build from this directory" call at the API
  level. It builds that tar in a blocking task (`tar::Builder::append_dir_all`)
  before ever touching the async `docker.build_image` call. It then picks one
  of *two* builders off that same tar: the classic one, or BuildKit when the
  build declares secrets or ssh forwarding. See [[build-inputs]] for why the
  split exists rather than one uniform path, and for the two things bollard's
  BuildKit driver forces on the caller (a process-global `SSH_AUTH_SOCK` and a
  `!Send` solve future).
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

## Volumes: two shapes, one Docker primitive

`#Volume` (`schema/component.cue`) is a CUE disjunction: a bind mount
(`{host, container, read_only}`) or a named volume (`{name, container,
read_only, shared}`), never both — `resolver::VolumeMount` mirrors this
directly as an untagged serde enum (`Bind` / `Named`), same pattern as
`Environment`'s map-or-list. Both variants land in the exact same place at
the Docker layer: `HostConfig.binds` accepts `"host/path:container/path"`
*or* `"volume-name:container/path"` in the same string list — Docker
disambiguates by whether the left side contains a `/` — so `RunOpts.binds:
&[String]` carries both, formatted by `start_node` before the container is
created.

A `Bind`'s `host`, if relative, resolves against the checkout root
`start_node` already has in hand for the build (`volume_base` — the live
workspace path the image was actually built from). It is **not** sandboxed to that repo — an
absolute or `..`-escaping path passes straight through, a deliberate
choice (a real-world dry run found it) to support the
pattern of bind-mounting a sibling repo's checkout directly (e.g.
`aikido-core`'s `php` service mounting `../intel`).

A `Named` volume's real Docker name is *derived*, never author-declared —
same invariant as a node's domain (see [[node-identity-and-domains]]).
`orchestrator::derive_volume_name` reuses `derive_domain` itself, so the
workspace name is folded in. It also folds in **the declaring node's id**, which is
what makes the author-chosen `name` private to the node that wrote it —
`data` in one repo and `data` in another are two volumes, exactly as two
services both called `api` are two nodes.

That qualification is newer than the rest of this page, and the reasoning
is worth keeping. Volume names were originally keyed on the bare `name`,
described here as transparent sharing. The sharing is genuinely useful, but
it was *unbounded and cross-repo*: two peer repos each declaring
`{name: data}` on their own Postgres got one volume with two
database engines writing to it. Silent data corruption, reachable from two
individually valid configs written by teams who have never spoken — the exact
scenario the whole id-qualification scheme exists to prevent, in the one
namespace that wasn't qualified, and the only one with a destructive failure
mode. See `concepts/AUDIT.md` B3.

B3's fix closed the *cross-repo* half of that hazard. A *cross-run* half
(`concepts/AUDIT.md` B16) — the default run's Postgres and a named run's
Postgres mounting one `scope: stable` volume at the same time — went away
with named runs and volume `scope`: there is one environment per workspace,
so one container per node, so one writer per volume.

Sharing is still expressible, now as `#Volume.shared: true`, which drops the
node-id qualification so the label alone decides identity. Reach for it
rarely. The usual reason to want it — several services behind one database —
is already `kind: shared-backing`, which gives you one *node*, and therefore
one container and one volume, without any cross-node name coincidence being
load-bearing.

Note that `shared: true` and `shared: false` derive different names, so
flipping it is a visible migration rather than a silent adoption of somebody
else's data. The same is true of the qualification change itself: volumes
created before it are orphaned, not re-pointed.

Named volumes are otherwise Docker-implicit — the daemon auto-creates one,
unlabeled, the first time a container's `Binds` references a name that
doesn't exist yet. `docker::ensure_volume` creates it explicitly instead,
right before use in `Orchestrator::start_node`, attaching the usual
`com.docker.compose.project` label (the workspace name).
`docker::list_volumes` finds them again by that label, which is how
`EnvState::volumes` gets populated, and how removing the workspace finds
them to delete. Nothing else deletes them: stopping and switching leave them,
because they hold the data the next start expects to find. Removing a
workspace is discarding it, so its volumes go, along with the anonymous ones
its containers' images declared (`docker::discard` removes a container with
`v`).

## Stopping a container is not the same as deleting it

Every teardown path in `fghj` goes through `docker::stop_and_remove`: the
workspace removal's teardown, the per-node `restart_node`, and
the sidecar. For a long time that
function was a single call — `remove_container` with `force: true`.

`force: true` is SIGKILL with no grace period at all. That is defensible for
a container you are deleting *because* it is broken, and indefensible for
every other caller on that list: stopping the environment, or restarting one node, is
an ordinary action a user takes several times a day. A dev Postgres with an
hour of seeded state in a named volume was being killed mid-write
by a routine click — which directly contradicts what a stable volume
promises. The whole point of a named volume is that its data outlives the
container; killing the writer without warning throws that away one
`fsync` at a time.

So `stop_and_remove` is now two calls: stop, then remove. The remove still
passes `force`, because a container that somehow survived the stop must not
hold its name, network or volumes hostage from the next start — but by then
the process has already had its chance to exit cleanly.

### Why the policy lives on the container, not on the call

The signal and the grace period are declarable — `#RunOptions.stop_signal`
and `#RunOptions.stop_grace_period`, available on `#Service`,
`#BackingDependency` and `#Task` alike. The obvious implementation is to
pass them to each `stop_container` call. fghj does the opposite: it stamps
them onto the container at **create** time, as
`ContainerCreateBody.StopSignal` / `StopTimeout`, and then calls
`stop_container` with no options at all.

The reason is that the set of things that stop a container is larger than the
set of things holding a `Node`:

- an **orphan** — the node was deleted from `.fghj.yaml`, so there is no
  config left to read a grace period out of, but the container is still
  running and will still eventually be stopped;
- a container that **outlived a daemon restart**, or whose SQLite row is
  gone;
- `docker stop` typed **by hand**, or Docker Desktop's stop button, or
  `docker compose down` against the project label fghj sets;
- fghj code added later that stops a container from a path nobody thought to
  thread the options through.

Recording the policy on the container makes every one of those honour it,
including the ones that are not fghj's code. This is the same reasoning as
the `com.docker.compose.*` labels a few sections up: state the intent where
Docker itself will act on it, rather than where only fghj can see it.

The cost is that changing either knob in `.fghj.yaml` does nothing until the
container is recreated. That is drift in the exact sense
[[config-drift]] means, so both fields are in `spec_hash` — see
`changing_a_stop_knob_changes_the_hash`.

The grace period defaults to Docker's own 10 seconds, so a config that says
nothing about stopping behaves exactly as it did before the fields existed.
The run's sidecar is the one container that asks for *less* (2s): it holds no
state worth flushing, since it is rebuilt from `routes.json` on the next
start, and a whole-run teardown should not spend ten seconds waiting on a
proxy.

`stop_signal` is validated in the resolver rather than left to Docker.
`resolver::validate::check_stop_signal` requires the `SIG`-prefixed
uppercase form and raises a **blocking** warning otherwise. Docker itself
would also accept `TERM` or a bare number, and `#RunOptions.stop_signal`'s
CUE pattern does not — so rather than have the two disagree, the Rust check
enforces exactly what CUE accepts and the error names the field and the fix
("write SIGTERM"), instead of Docker's own bare `invalid signal`. See
[[config-language]] on which of the two is allowed to be stricter.

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
synchronous HTTP handler. `downloads::start_pull_all` / `start_node` run
each job as an `activity::Activity` (kind `pull`) on its own OS thread (not a tokio task — `git clone`'s own process spawn and
blocking I/O don't need the async runtime) and lets the UI poll for
progress instead:

- **Keys**: `"node:<id>"` for a single-node download, `"pull-all"` for the
  whole-graph fixpoint pull, `"pull-flow:<flow>"` for a flow-scoped one
  (`pull_all_key`, shared between `start_pull_all` and the status-lookup
  handler so both agree on how a flow name becomes a key). A flow-scoped
  pull and the whole-graph one are tracked independently, so both can be
  polled — or even run — at once without one clobbering the other's
  status.
- **Idempotent starts**: `ActivityLog::start` checks whether a job under
  this key is already running in this workspace and, if so, hands back its
  snapshot instead of starting a second concurrent clone of the same thing.
  A finished job under the key is replaced, so each key keeps one record.
- **Children**: `pull_all_logged` makes each clone a child job keyed by the
  folder it lands in. A child's output is appended to its parent's log too,
  so the parent's log stays the whole transcript.
- **Images**: once the clones stop turning up anything new, so every node in
  scope is known, `pull_images_logged` adds an `images` child holding one
  queued child per image, keyed by its reference: each image a node runs as
  is (`image:`, not `build:`, since BuildKit fetches a build's bases itself)
  that the machine lacks, once per platform. They're listed before the first
  starts, then pulled in turn through fghjd's own Docker client, the job
  thread driving it on the daemon's runtime. A child's log is its
  `$ docker pull` line, each layer's milestones and the digest, not every
  progress tick. A single-node pull pulls that node's image the same way.
  The Engine API never pulls on its own (creating a container from a
  missing image is a 404), so a pull is what makes `fghj up` only build.
  Starting a node still pulls a missing image as a step of its own, for one
  started without a pull or whose image changed or was pruned since.
- **Log streaming**: `run_git_clone_logged` pipes the child's stdout *and*
  stderr (git's own progress meter writes to stderr, not stdout) into the
  same growing log string the UI polls, translating `\r` (a real terminal's
  "overwrite this line" convention for progress meters) into `\n` since
  that reads far better inside a `<pre>` block than a wall of
  carriage-returns.
- **Ordering**: the log is a `Vec`, not a map, so `ActivityLog::list` can
  return jobs newest-first by start rather than by key.
- **Scope**: the log is the daemon's, shared by every workspace, and in
  memory only — a restart forgets finished pulls, which are safe to run
  again.
- **Queued steps**: `queued_child` lists a step before it runs (status
  `queued`) so the plan shows up front; `step` begins it, runs it and settles
  it. A parent settling cancels its queued steps like its running ones.
- **Other kinds**: the same log holds workspace removals (`remove`, see
  [[run-lifecycle-and-registry]], in three queued steps: containers, network,
  volumes) and the converge effect's `switch`,
  `start`, `stop` and `delete` jobs (see [[state-and-effects]]). A handle
  whose job was since replaced writes nowhere instead of panicking: a
  superseded switch's task can still be reporting.

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

## Privilege drop for clones

`run_git_clone_logged` takes an optional `&WorkspaceOwner` and applies it
(see [[persistence-and-workspace-store]] for the full owner-capture story),
plus `store::harden_git_ssh` on top, as defense in depth against a
`git clone` subprocess hanging on an unanswerable interactive prompt. (The
resolver used to have a second privilege-dropped clone path,
`ensure_mirror`, for the now-removed branch-override build path — see
[[run-lifecycle-and-registry]] — that function no longer exists.)

## Status

Implemented: `src/docker/` (build/run/inspect/logs/`stop_and_remove` over
`bollard`),
`src/downloads.rs` (single-node/pull-all/pull-flow jobs, log streaming) on
`src/activity.rs`. `daemon/` exposes all of it over HTTP (see
[[control-api-and-cli]]); `OperationsDrawer.svelte` and `Drawer.svelte` are
the two UI surfaces that poll it (see [[ui-architecture]]).
