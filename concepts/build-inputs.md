# Build inputs: args, target, secrets and the forwarded agent

> **Status:** implemented. Language in `schema/component.cue` (`#Build`,
> `#BuildSecret`) and `src/resolver/` (`config::Build`, `graph::NodeBuild`,
> `graph::NodeBuildSecret`); transport in `src/docker.rs`
> (`BuildOpts`, `build_image`, `build_image_buildkit`,
> `solve_on_dedicated_thread`, `SSH_AUTH_SOCK_ENV`); resolution in
> `src/runs/node_spec.rs` (`build_node_image`, `resolve_build_secrets`);
> drift in `src/runs/spec.rs`. Closes [[AUDIT]] E3.

## The hole this fills

`#Build` was exactly `{context, dockerfile, args}`. No `target`, no `secrets`,
no `ssh`. Any Dockerfile doing `RUN --mount=type=ssh`, or pulling from a
private npm/Go/Cargo registry, **could not be built by fghj at all** — which
for the stated audience (a company with ~15 private sibling repos) is the most
common hard blocker there is.

The irony was structural. [[persistence-and-workspace-store]] describes an
elaborate `WorkspaceOwner` mechanism whose entire purpose is to lend `fghjd`
— a root daemon with no credentials of its own — the developer's live
ssh-agent so `git clone` can reach a private repo. None of it reached
`docker build`. fghj could *fetch* a private repo and then fail to *build* it.

Two live bugs surfaced while closing this, neither named by the audit:

- **`build.args` never reached the daemon.** It was parsed from YAML, carried
  into `NodeBuild`, and then dropped: `build_image` took `(dir, dockerfile,
  tag, platform)` and nothing else. A config that set it was silently ignored.
- **`build` was absent from `spec_hash`.** Editing the build block changed
  what the next build would produce while leaving the drift hash identical,
  so nothing downstream ever noticed.

## The language

```yaml
services:
  api:
    build:
      context: .
      dockerfile: Dockerfile
      target: dev            # which stage of a multi-stage Dockerfile
      ssh: true              # forward the workspace owner's agent
      args:
        RUST_VERSION: "1.94"
      secrets:
        - id: npmrc          # what the Dockerfile's RUN --mount names
          file: ci/.npmrc    # resolved against the repo checkout root
```

Every field is optional and every default is the old behaviour, so a config
written before these existed resolves to exactly the build it always did.

`target` is the one that needed no new machinery at all — every builder has
always supported it, it simply was not plumbed. `target: dev` against
`target: prod` off one multi-stage Dockerfile is the standard pattern; without
it a repo needs a second Dockerfile.

`secrets[].file` resolves against the repo checkout root, exactly like
`#Volume`'s bind `host` and `#RunOptions.env_file`. A `file` that does not
exist (or is a directory) fails the node **before** the build starts. The
alternative — passing an empty secret through — puts the failure inside the
Dockerfile, where it surfaces as a 401 from some registry three `RUN` steps
later, which is a long way from "you forgot to create the file".

There is deliberately **no `env:` secret source**, though BuildKit supports
one. `fghjd` is a root daemon with no access to the developer's shell
environment, so there is no environment to read one from. That is the same gap
[[AUDIT]] E2 records, and it has to close first.

## Why `ssh` is a boolean, not a path

`#Build.ssh: true` says *forward the workspace owner's agent*, and nothing
else. BuildKit's ssh forwarding is keyed by socket id, and bollard's provider
accepts only the id `default` — but more importantly, letting a repo name a
socket path would be letting a repo name a credential, which is the one thing
a federated per-repo config file must not be able to do. The path comes from
`WorkspaceOwner`, re-derived on every build by `live_ssh_auth_sock` rather
than read from the value stored at `fghj wire` time, so a restarted agent is
picked up — the same reasoning `apply_to_command` already followed for
`git clone`.

If `ssh: true` is set and no live agent can be found, the node fails with a
message naming `fghj wire`. Building anyway would produce a Dockerfile-level
failure that looks like a network problem.

## One builder

`build_image` goes through BuildKit, always. There is no dispatch and no
second path.

There used to be. `BuildOpts::needs_buildkit()` chose the classic builder
for any build declaring neither `secrets` nor `ssh`, on the grounds that
bollard's BuildKit driver collapses an entire solve into one
`Result<(), GrpcError>` — no progress stream, and a much blunter error than
the classic path's `error_detail.message`, which comes straight out of the
daemon and names the failing step. Keeping the better errors for the builds
that didn't need BuildKit looked like a free win.

It wasn't free. It meant `RUN --mount=type=cache` — the single most useful
BuildKit feature for a dev loop, and the thing that turns a Go or Node
rebuild from a minute into seconds — worked only in repos that happened to
declare a secret for some unrelated reason. Two builders also means two sets
of behavior to reason about for every other feature: a build that works today
can break tomorrow because someone added `ssh: true` and silently moved it to
the other builder.

BuildKit has been the default builder in Docker Engine since 23.0 (February
2023), so the compatibility argument for keeping the classic path had already
expired. A daemon too old to offer BuildKit is now a failed build with a
message saying so, rather than a silently different one — and `fghj doctor`
([[preflight-checks]]) reports it up front instead of waiting for the first
build to discover it.

The blunter error message is the price, paid uniformly. It's visible in
`build_image_buildkit`'s own error context rather than hidden behind a
condition nobody can predict.

### The price may have stopped being necessary

Worth revisiting, because the reasoning above rests on a premise that has
since changed. The classic path was rejected as "not BuildKit" — no
`--mount=type=cache` — which made the choice a genuine dilemma: good errors
*or* a fast dev loop.

bollard then merged `build_image_with_session_providers` (PR #731, August
2026), which serves `--mount=type=secret` and `--mount=type=ssh` over the
*legacy streaming* `/build` endpoint when the build asks for
`BuilderVersion::BuilderBuildKit`. That endpoint is real BuildKit, so cache
mounts work, and it still returns the `BuildInfo` stream with
`error_detail.message` and the step output. On paper that is the same engine
the `Moby` driver reaches, with the same features, plus the logs — which would
make the trade this section describes unnecessary rather than merely painful.

Two things keep it from being a free swap, and neither has been measured:

- It can only reach **dockerd's embedded** BuildKit, never a buildx
  container, so taking it would undo [[builder-parity]] — which exists
  because those two engines disagree about real Dockerfiles. Logs *or* the
  right engine.
- Whether it carries everything the driver path does (platforms,
  cache-to/from) is unverified.

The unblocked version of this is a progress hook on the driver path itself,
which bollard's maintainer has invited a PR for since
[issue #454](https://github.com/fussybeaver/bollard/issues/454) (August 2024,
still open): *"the progress hook in the 'driver'-based build path isn't
implemented yet... I'm happy if there's any interest in taking a stab at
that."* Nobody has. That is the fix worth doing; it is deferred, not
rejected.

The built image lands in the daemon's local image store under the requested
tag either way, so `create_container`, `spec_hash`'s image field and
everything else downstream see exactly what they did before. *How* it gets
there depends on the driver: the `Moby` driver asks for the `docker` exporter
and the image appears in the store directly, while a container-backed builder
([[builder-parity]]) has no access to that store at all and must export a
docker-format tarball that fghj streams back through `/images/load`.

## The context is filtered client-side, or not at all

`.dockerignore` is a **client-side** convention. The Docker CLI applies it
before uploading; the daemon and BuildKit only ever see the tarball they are
given. fghj tars the context itself — the Engine API has no "build from this
path" call ([[docker-and-downloads]]) — so not implementing the file means not
honouring it, silently.

fghj did not honour it, and the symptom was not the obvious one. A Go repo
whose `.dockerignore` excluded `.git` built on the command line and failed
under fghj with:

```
error obtaining VCS status: exit status 128
	Use -buildvcs=false to disable VCS stamping.
```

Go stamps VCS information into a binary whenever it finds a repository beside
the source. `git` inside the builder cannot read a `.git` that arrived through
a tarball, so it exits 128 and takes the build with it. Nothing about the
error mentions the context; it names the compile step, so the Dockerfile is
where anyone looks first. The same repo also shipped 572 MB of `.local` jars —
including a private key and a production JWT — into image layers on every
build, and a second repo uploaded 209 MiB where 2 MiB was warranted.

Implemented in `src/dockerignore.rs`, applied by `append_filtered` in
`src/docker.rs`.

### Why this is hand-rolled

A new matcher is hard to justify when `ignore` exists and has 184M downloads,
so: **`.dockerignore` is not `.gitignore`.** Its patterns are anchored at the
context root. A bare `node_modules` excludes the top-level one and leaves
`pkg/node_modules` alone; gitignore matches a bare name at any depth and would
exclude both. Verified against a real daemon rather than taken from the docs —
a build whose `.dockerignore` held only `node_modules` received
`pkg/node_modules/n.txt` and not `node_modules/r.txt`.

Borrowing gitignore semantics would therefore make fghj filter contexts
differently from `docker build`, which is precisely the failure
[[builder-parity]] exists to eliminate: fghj and the CLI disagreeing about one
repo, with nothing in the repo to explain it. The only published
`.dockerignore` crate is `use-dockerignore`, at 0.0.1 and 153 downloads.
`globset` could replace the per-segment matching, at the cost of pulling
`regex-automata` into a dependency tree deliberately kept lean; the precedence
and ancestor rules would still have to be written here.

Two rules are worth knowing because getting either wrong breaks a build rather
than merely over-shipping:

- **The Dockerfile is always sent, even when the file excludes it.** Real
  `.dockerignore` files do list `Dockerfile`, since `COPY . .` would otherwise
  bake it into the image. Honouring that literally would leave BuildKit
  nothing to build. The CLI makes the same exception, for `.dockerignore`
  itself too.
- **An excluded directory is not walked** unless some `!` pattern could
  re-include a file inside it. That fast path is what keeps a 232 MB
  `node_modules` from costing anything, and `Dockerignore::has_exclusions`
  exists only to decide it.

A context with no `.dockerignore` keeps `tar`'s own bulk walk, so every
existing build is byte-for-byte unchanged. An unreadable or empty file
excludes nothing: a context fghj cannot filter ships too much, which is far
easier to diagnose than a build that silently lost its sources.

## Two things bollard forced

Both are worth recording because neither is visible from the call site.

**The agent socket is process-global.** bollard's `SshProvider` reads
`SSH_AUTH_SOCK` from *this process's* environment (`check_agent` in
`bollard::grpc`), with no way to pass one in. `fghjd` runs as root and has no
agent, so the only way to lend it the owner's is to set the variable around
the build. That is global to the process, hence `SSH_AUTH_SOCK_ENV`: a
`tokio::sync::Mutex` held for the whole build, restoring the previous value
after. Two ssh-forwarding builds therefore never overlap. Builds that do not
forward ssh never touch the variable and stay fully concurrent, which is the
common case. `set_var` is `unsafe` in edition 2024 because it races other
threads reading the environment; the lock makes fghjd's own accesses
exclusive, and the read that matters happens inside the build it brackets.

**The solve future is `!Send`.** `Build::docker_build`'s driver tear-down
handler is a bare `Box<dyn Future>`, so awaiting it inline would make every
caller up the stack `!Send` — including the `tokio::spawn` in
`daemon::reconcile`. Rather than restructure the daemon around one
dependency's boxed future, `on_dedicated_thread` gives the solve a
current-thread runtime and a `LocalSet` on a thread of its own and sends only
the result back over a oneshot.

Enabling the feature also needs `features = ["buildkit", "time"]`: bollard's
`buildkit` feature does not itself pull in a date-time provider that its own
`AuthProvider` requires, so `buildkit` alone does not compile. `ssl` resolves
to `rustls/ring`, the provider fghj already uses, so there is no conflict.

## Build inputs and drift

`build` is now part of `spec_hash`'s `DesiredSpec`. It has to be: the built
image's *tag* is stable across rebuilds (`fghj/<id>:<branch>`), so the tag
already in the hash cannot distinguish two different images.

This does **not** make the hash content-aware. Editing the Dockerfile itself,
or any file in the build context, still slips through — that is [[AUDIT]] B13,
and it stays open. What closed is narrower and worth stating precisely:
changes to the *declared* build inputs are now visible to drift detection.

This is the same trap [[terminating-nodes]] avoided when it declined to key
`run: once` on the config hash.

## Related

- [[docker-and-downloads]] — where `build_image` sits in the Docker layer, and
  the `WorkspaceOwner` forwarding that `git clone` already used.
- [[persistence-and-workspace-store]] — `WorkspaceOwner`, `fghj wire`, and why
  a root daemon needs to borrow a user's agent at all.
- [[run-lifecycle-and-registry]] — where `build_node_image` runs inside
  `start_node`.
- [[terminating-nodes]] — an image-less task builds under its *owner's* tag,
  so these inputs apply to it unchanged.
- [[AUDIT]] — E3 (closed here), E2 (blocks `secrets: env:`), B13 (build
  context contents still unhashed).
