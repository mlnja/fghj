# Builder parity: build where `docker build` would have

> Implemented in `src/buildx.rs` (`default_builder`, `HostBuilder`);
> `src/docker.rs` (`BuildOpts::builder`, `solve_on_host_builder`,
> `buildkit_image_of`, `retire_builder_if_image_changed`, `load_image_tar`,
> `FGHJ_BUILDKIT_CONTAINER`); `src/runs/node_spec.rs` (`build_node_image`).

## The bug this closes

A user built a repo with `docker build` and it succeeded. `fghj` built the
same repo, from the same checkout, and it failed. Nothing in the repo
explained the difference, and the obvious conclusion — that `fghj` was
mangling the build — was wrong.

`docker build` does not necessarily use the BuildKit inside `dockerd`. When
`buildx` has a container-backed builder selected — OrbStack and Docker Desktop
both create one, and `docker buildx ls` marks it with a `*` — the CLI sends
the build to *that container*, which carries its own BuildKit release. On the
machine where this was diagnosed:

```
orb-builder *  docker-container   BuildKit v0.32.2
default        docker             BuildKit v0.29.0
```

`fghj` hardcoded the `Moby` driver, which means `dockerd`'s embedded BuildKit:
the second row. Three minor versions apart, and far enough apart to disagree
about whether a `RUN` may redirect output into a directory that does not exist
yet. The user's actual Dockerfile needed `mkdir -p /out` under v0.29.0 and not
under v0.32.2.

The failure mode is what makes this worth a document. Both engines are
correct, both are current, neither logs its own version, and the error
surfaces as a build failure attributable to anything. A user cannot
reasonably be expected to discover that their CLI and their tooling are
talking to two different build engines.

## The precept

**Build where the user's own `docker build` would have built.** Agreement
with the CLI on the same machine is worth more than any property fghj could
choose for itself, because disagreement is unattributable: the user has no
way to tell a bug in their Dockerfile from a bug in fghj.

## Resolution: read buildx's own state, as the owner

`buildx` keeps plain JSON under `~/.docker/buildx`, so this needs no
subprocess:

- `current` → `{"Key":"orbstack","Name":"orb-builder","Global":false}`
- `instances/<name>` → the driver and its nodes.

A `docker-container` driver resolves to a container named
`buildx_buildkit_<nodeName>`. That name is buildx's own fixed convention
(hardcoded in its `driver/docker-container/factory.go`), not something stored
in the instance file, so `container_name` reproduces it rather than reading
it. Any other driver — `docker`, `remote`, `kubernetes` — returns `None` and
fghj keeps its previous behaviour.

**The home directory must be the workspace owner's, never the process's.**
`fghjd` runs as root, so `$HOME` is `/var/root`, which holds no buildx config
at all. Reading the process's environment would resolve to "no builder
selected" on every machine and silently produce the exact wrong answer — the
bug, wearing the fix's clothes. `build_node_image` therefore loads
`WorkspaceOwner` first and passes `o.home`. This is the same hazard
[[persistence-and-workspace-store]] describes for `git`, and
`connect_docker()` in `src/daemon/bootstrap.rs` still has an open instance of
it.

## fghj runs its own BuildKit, at buildx's image

fghj does **not** build inside buildx's container. bollard stamps ownership
labels on containers and volumes it manages and refuses to adopt one it did
not create:

```
ownership label `com.github.fussybeaver.bollard.buildkit.managed` does not match
```

There is no opt-out, and that is the right guard rather than an obstacle to
route around. Stopping or reconfiguring the builder the user's own
`docker build` depends on is not fghj's to do.

So fghj runs a persistent container of its own, `fghj_buildkit`, with a
`fghj_buildkit_state` cache volume, pinned to the **same image** as buildx's.
Same engine, separate lifecycle.

### Pin by digest, not by tag

The image is resolved to a **repo digest**
(`moby/buildkit@sha256:…`) via `inspect_container` → image id →
`inspect_image().repo_digests`, falling back to the tag only if that fails.

This is not fastidiousness; matching tags is actively wrong.
`moby/buildkit:buildx-stable-1` *moves*. A container created from it a week
ago runs v0.32.2 while pulling the same tag today gets v0.33.1. An early
version of this feature compared `Config.Image` strings, found them equal,
and reported success while running a *different* BuildKit from the CLI — the
original bug with extra steps, and now invisible to every string comparison
available. The test therefore asserts on `buildkitd --version` executed
*inside* both containers, which is the property actually under test.

`retire_builder_if_image_changed` removes `fghj_buildkit` when the resolved
digest no longer matches, keeping the cache volume (`v: false`).

## The two costs, paid knowingly

- **A second build cache.** `fghj_buildkit_state` is not buildx's state
  volume, so the first fghj build of a repo is cold even if the CLI built it
  a minute earlier. Unavoidable given the ownership guard above, and the
  cheaper half of the trade.
- **Every image crosses the socket twice.** A BuildKit in a container has no
  access to `dockerd`'s image store — this is why `buildx` itself needs
  `--load` — and bollard has no `impl Build for DockerContainer` to hide it.
  So the solve exports a docker-format tarball to a tempdir and
  `load_image_tar` streams it back through `/images/load`. One extra copy of
  every built image, on the embedded path's zero.

The embedded path is unchanged and still taken whenever no container-backed
builder is selected, which is why `on_dedicated_thread` was generalised out of
`solve_on_dedicated_thread`: two solve shapes, one `!Send` workaround.

## Which engine ran is reported, always

The Events tab names it, because "which BuildKit built this" is the first
thing worth knowing when a build behaves differently from the CLI and the one
thing no error message mentions:

```
building image   fghj/app:main  ·  /Users/me/src/app  ·  buildkit: orb-builder
```

`buildkit: dockerd` on the embedded path. `BuildReport::builder` carries it.

## Status

Implemented. Verified against a real daemon by three `#[ignore]`d tests in
`src/docker.rs` (`mod build_tests`) covering: the image landing in dockerd's
store and running; container reuse across two builds with identical layer
digests, version parity with buildx, and buildx's own container and volume
left untouched; and `--build-arg`/`--target` surviving the tar round trip.

Open: `connect_docker()` runs `docker context inspect` as root, the same
owner/home bug in a different place.

## Related

- [[build-inputs]] — why every build goes through BuildKit at all, and the
  progress-stream cost that decision accepted. That cost is what makes the
  engine mismatch so hard to diagnose: without step output, a version
  disagreement looks like an ordinary build failure.
- [[docker-and-downloads]] — where `build_image` sits in the Docker layer.
- [[persistence-and-workspace-store]] — `WorkspaceOwner`, and why a root
  daemon must borrow the owner's home rather than read its own.
