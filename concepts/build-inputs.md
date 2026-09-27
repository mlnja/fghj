# Build inputs: args, target, secrets and the forwarded agent

> **Status:** implemented. Language in `schema/component.cue` (`#Build`,
> `#BuildSecret`) and `src/resolver/` (`config::Build`, `graph::NodeBuild`,
> `graph::NodeBuildSecret`); transport in `src/docker.rs`
> (`BuildOpts`, `build_image`, `build_image_classic`, `build_image_buildkit`,
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

`target` is the one that needed no new machinery at all — the classic builder
has always supported it, it simply was not plumbed. `target: dev` against
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

## Two builders, chosen per build

`build_image` dispatches on `BuildOpts::needs_buildkit()`:

| declares | builder | why |
|---|---|---|
| neither `secrets` nor `ssh` | classic | `args`, `target` and `platform` all work there |
| either one | BuildKit | nothing else can mount a secret or a socket |

Uniformity was the obvious alternative and was rejected. bollard's BuildKit
driver collapses an entire solve into one `Result<(), GrpcError>` — no
progress stream, and a much blunter error than the classic path's
`error_detail.message`, which comes straight out of the daemon and names the
failing step. Every repo that exists today builds fine on the classic path, so
they keep its better errors, and only a build that actually demands BuildKit
pays for it.

The cost of the split is real and worth naming: a Dockerfile using
`RUN --mount=type=cache` works only if that build *also* declares a secret or
`ssh`. Nothing forces BuildKit on otherwise. An explicit opt-in knob is the
obvious follow-up if that bites.

The built image lands in the daemon's local image store under the same tag
either way — the `Moby` driver asks for the `docker` exporter — so
`create_container`, `spec_hash`'s image field and everything else downstream
are unchanged by which path ran.

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
dependency's boxed future, `solve_on_dedicated_thread` gives the solve a
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
