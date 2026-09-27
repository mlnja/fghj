# The in-network sidecar: one TLS proxy per run

> **Status:** implemented. The binary is `src/bin/fghj-sidecar.rs`; its image
> is built by `src/sidecar_image.rs`; its lifecycle by `src/runs/sidecar.rs`
> (`ensure_sidecar`) and `src/runs/orchestrate.rs`; its route table by
> `src/effects/routes.rs` and `src/runs/route_table.rs`. Closes [[AUDIT]] D2's
> sidecar gap — the subsystem existed in `docs/` but not in `concepts/`.

## The hole this fills

[[local-ca-and-tls-proxy]] gets a *browser* to
`https://cart.myworkspace.fghj.internal`: the OS resolver is pointed at fghj's
DNS, which answers `127.0.0.1`, where a TLS proxy terminates with a
locally-trusted cert and dispatches by SNI.

None of that exists inside a Docker network. A container asking for the same
name gets Docker's embedded resolver, which has never heard of it; and even if
it resolved, `127.0.0.1` inside a container is the container itself.

So the two halves of a stack would have had to address each other differently:
a browser by proxied name, a sibling container by… something else. Every
`.fghj.yaml` would encode which side of the network boundary the caller is on,
which is exactly the thing an environment tool should be hiding.

The sidecar makes the addressing identical from both sides. One container per
run, on that run's network, running **the same proxy and CA code the host
daemon runs** — `web::proxy` and `web::ca`, imported unmodified.

## Why a separate binary

`fghjd` could have grown a `--sidecar` flag. It did not, for one reason worth
stating plainly: this container gets **the CA's private key bind-mounted in**.
That deserves a minimal entrypoint someone can audit in one sitting, not a
branch inside a binary that also requires root, installs trust stores, edits
`/etc/pf.conf` and serves the full control API.

It also has no CLI arguments and no configuration. Every path is a hardcoded
constant (`/etc/fghj-sidecar/ca`, `/etc/fghj-sidecar/routes/routes.json`), so
the only thing that ever has to agree with this binary is the bind-mount list
in `ensure_sidecar` — one place, checkable by reading two files.

## How it learns the routes

`fghjd` writes a `routes.json` per network; the sidecar polls it once a
second and rebuilds its table when the mtime changes.

**Polling, not inotify** — and not because polling was easier. Bind-mount
filesystem event propagation across the OrbStack / Docker-Desktop
virtualization boundary is unreliable, which is precisely the class of
platform-specific gap this subsystem exists to avoid. A 1s poll of one small
JSON file costs nothing and has no missed-event failure mode.

The file is a pure function of `RunState::containers`, rendered by
`effects::routes` off published state. It used to be written by hand at the
end of each lifecycle call, from a `RunState` snapshot taken *before* the
Docker work — which meant it could be rendered from a snapshot that had
already lost a sibling's routes. Driving it from state removed that class of
bug entirely; see [[state-and-effects]].

Each entry is `{lookup, wildcard, connect_host, connect_port}`. The
`connect_host` is the backend's own **`fghj.raw.internal`** domain — a real
Docker network alias that Docker's embedded DNS already resolves for any
container on the network, this one included. So the sidecar never needs to
learn container IPs, and never goes stale when one changes.

`RouteFileEntry` is defined separately in the sidecar rather than shared with
`fghj::runs`, with matching field names, so the sidecar binary does not depend
on the run machinery at all.

## Three things it does at once

**It is the proxy.** `serve_https` with a `DynamicCertResolver` over the
mounted CA, plus `serve_http_redirect` on port 80 — the same pair the host
runs. Exact route matches beat wildcard matches, the same precedence
`daemon::routing`'s resolver uses.

**It is the DNS authority for `fghj.internal` inside the run.** Every node
points `--dns` at this container first, so no per-consumer opt-in is needed.
Its authority set is *exactly its route table* — a name is "ours" the instant
it is routable, with no separate zone bookkeeping that could disagree with
what the proxy will actually serve. Anything it does not recognize is
forwarded verbatim to Docker's embedded resolver at `127.0.0.11`, which is how
`fghj.raw.internal` names and ordinary internet names keep working. See
[[two-zones-and-raw-ports]].

**It answers with its own address.** Not a per-backend IP — the whole point is
that in-network addressing mirrors host addressing: one address, dispatch by
name after the connection lands.

It discovers that address without an env var, a DNS lookup, or a dependency on
anything else being up: bind a UDP socket, `connect` it to an unreachable
address, and read back `local_addr()`. No packet is sent; the kernel just
picks the source address it *would* use, which is this container's real
in-network IP.

## Lifecycle

`ensure_sidecar` runs before any node in the run. If a sidecar for the run is
already alive and has an IP on the network, it is reused; otherwise any
stopped-but-not-removed predecessor is force-removed first (its name is fixed,
`fghj-{workspace}-{run}-sidecar`, so a leftover would collide) and a fresh one
started. If the whole run's start fails, the sidecar is torn down with it.

Its IP is threaded into every node's `--dns` at `start_node` time, which is
why it must come up first.

`sidecar_image::ensure_built` makes sure the image exists first — see *Where
the image comes from* below.

Two bind-mount details that cost real debugging time and are worth keeping:

- **Paths are canonicalized before mounting.** macOS's `/var` is a symlink to
  `/private/var`, and OrbStack's bind-mount source resolution does not follow
  it — a source of `/var/lib/fghjd/...` silently resolves *inside the Docker
  VM's* filesystem instead of the host's, so the mount appears empty rather
  than failing. The pre-resolved `/private/var/lib/...` form mounts correctly.
- **The two mounts are siblings, not nested.** Binding `ca` underneath an
  already read-only bind-mounted `/etc/fghj-sidecar` fails outright: the
  runtime cannot create a mountpoint inside a read-only mount. Leaving
  `/etc/fghj-sidecar` itself unmounted lets the runtime create it as an
  ordinary writable directory in the container's own layer, and both binds
  attach under it independently.

## Where the image comes from

The sidecar is the one image fghj has to supply itself, and for a long time it
supplied it the only way a single binary with no cargo workspace on the target
machine can: by building it. `ensure_built` now tries three things in order.

**1. Is it already here?** `inspect_image` on the exact tag. Cheap, and the
reason the escape hatch below sticks.

**2. Pull the published build.** The release workflow builds
`sidecar/Dockerfile` and pushes it to `ghcr.io/mlnja/fghj-sidecar`, tagged with
the crate version. This is the normal path for anyone who installed a release.

**3. Build from the embedded source.** The original path, kept rather than
replaced. `fghjd` ships as a prebuilt binary with no cargo workspace on the
target machine, so the build context cannot be "the repo on disk" — the crate
is embedded into `fghjd` at compile time with `include_dir!` (the same trick
`web::ui` uses for the UI) and materialized to a scratch directory.

### Why the fallback stays

Step 3 is not dead weight. Three ordinary situations reach it:

- `cargo run` from a working tree whose version was never released — which is
  every working tree between two releases, i.e. all development.
- A machine with no route to `ghcr.io`.
- A fork that doesn't publish to this registry.

None of those should be fatal when the source is sitting right there in the
binary. The pull failing is logged at *info*, not warn: for an unreleased
version it is the expected outcome, and the build that follows is the real
answer. Only that build's failure is an error.

### Why the registry is in the tag

`image_tag()` returns the fully-qualified `ghcr.io/mlnja/fghj-sidecar:x.y.z`,
not a bare `fghj-sidecar:x.y.z` that gets re-tagged after a pull.

This matters because a pulled image lands in the local store under the
reference it was pulled by. If the daemon asked Docker for a bare name, step 1
would keep missing after a successful step 2, and every daemon start would
re-pull. One string for a sidecar image, everywhere, is what keeps the
"already present?" check from disagreeing with what `runs/` starts.

The registry is a constant rather than a setting on purpose. This is not a
user-chosen image: its source is in this crate and its version must match the
daemon's exactly. A knob would only let someone point fghjd at a sidecar that
doesn't speak the same `routes.json`.

There is a matching literal in `.github/workflows/release.yml`
(`SIDECAR_IMAGE`) rather than a templated `github.repository_owner`. A fork
that templated it would push its sidecar to its own namespace while its
`fghjd` kept pulling ours — and because the crate version would usually match,
the wrong image would arrive with no error at all. Forking the sidecar means
editing both places, which is the honest cost.

### Why CI builds it once per architecture

The release workflow runs the build natively on `ubuntu-latest` and
`ubuntu-24.04-arm`, pushes each result **by digest with no tag**, and a
separate job joins the digests into one manifest list with
`docker buildx imagetools create`.

The obvious alternative — one buildx run with
`--platform linux/amd64,linux/arm64` — emulates the non-native half under
QEMU. That half is a full release compile of this crate, which turns a
couple of minutes into most of an hour. Both architectures are needed because
both are shipped: Docker on an Apple Silicon Mac runs `linux/arm64`, on an
Intel Mac `linux/amd64`, and the release has assets for both.

Pushing by digest rather than by tag is not a detail. Two runners pushing the
same tag in parallel is just the loser overwriting the winner, and what
survives is a single-architecture image that half the users can't run.

`create-release` is gated on the manifest job. A release whose sidecar image
is missing still works — step 3 catches it — but it silently hands every new
user the slow first run this whole arrangement exists to remove, which is
worse than not cutting the release.

### The one escape hatch

Iterating on the sidecar's own source used to mean deleting the cached image
tag to force a rebuild. With a pull in the way, that would now fetch the
published image over your changes. `FGHJ_SIDECAR_LOCAL_BUILD=1` skips step 2.

That is the only knob, and it exists for the person editing
`src/bin/fghj-sidecar.rs`, not for configuration.

## What it deliberately does not have

No control API. Its `control_port` is `0`, because nothing inside a run's
network ever presents the zone apex's own SNI, so that branch is never
reached. The sidecar serves this run's services and nothing else.

## Related

- [[local-ca-and-tls-proxy]] — the proxy and CA code this reuses verbatim, and
  the cert-eligibility rule it inherits.
- [[two-zones-and-raw-ports]] — why `connect_host` is a raw-zone name and what
  the sidecar forwards rather than answers.
- [[run-lifecycle-and-registry]] — where `ensure_sidecar` sits in a run's start.
- [[state-and-effects]] — `effects::routes` as the writer of `routes.json`.
