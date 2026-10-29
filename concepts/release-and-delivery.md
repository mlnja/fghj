# Release and delivery: what "installed fghj" actually means

> **Status:** implemented. `Justfile` (`release`, `update-tap`),
> `.github/workflows/ci.yml`, `.github/workflows/release.yml`,
> `src/sidecar_image.rs`, `[package.metadata.deb]` and `packaging/`,
> `Formula/fghj.rb` in the separate `mlnja/homebrew-tap` repo, and the
> separate `mlnja/apt` repo behind apt.mlops.ninja. The known gaps are listed at the bottom and
> tracked in [[AUDIT]].

Every other file in this folder describes how fghj behaves once it is running.
This one describes how it gets onto a machine, because that turned out to have
its own set of non-obvious constraints — and one of them was quietly broken.

## Four artifacts, one version

An installed fghj is not one thing:

| Artifact | Where it comes from | Where it goes |
|---|---|---|
| `fghj` (CLI) | `release.yml`'s `build` job (macOS) and `deb` job (Ubuntu) | `bin/` via Homebrew; `/usr/bin` via the `.deb` |
| `fghjd` (root daemon) | same | the same, plus a LaunchDaemon or the `fghjd` systemd unit |
| `ghcr.io/mlnja/fghj-sidecar` | `release.yml`'s linux matrix | the user's Docker image store |
| the UI | compiled *into* both binaries | nowhere — see below |

All four are keyed to one number: the `version` in `Cargo.toml`. The CLI and
daemon carry it as `CARGO_PKG_VERSION`; the sidecar image is tagged with it
(see [[in-network-sidecar]] for why the reference is registry-qualified); the
Homebrew formula pins it, and the `.deb` is `fghj_<version>-1_<arch>`. `just release` bumps it, commits, tags, and pushes,
and the tag is what fires `release.yml`.

The sidecar tag is read from `Cargo.toml` in CI rather than from the git tag
that triggered the workflow. This looks like a pointless indirection and isn't:
`image_tag()` is built from `CARGO_PKG_VERSION`, so `Cargo.toml` is the only
source that cannot drift away from what the shipped binary will ask Docker for.
A mistyped git tag should produce a mislabelled *release*, not a daemon that
pulls an image nobody published.

## The UI is a compile-time dependency, and CI didn't know

`web::ui` embeds the built Svelte app with
`include_dir!("$CARGO_MANIFEST_DIR/ui/dist")`. `ui/dist` is gitignored
(`ui/.gitignore`), and `include_dir!` is a proc macro that **panics at compile
time** when its directory is missing:

```
error: proc macro panicked
  = help: message: ".../ui/dist" is not a directory
```

So on a clean checkout — which is exactly what a CI runner has — nothing
compiled. Not the check job, not the release build, not the sidecar image.
Both workflows now run `npm ci && npm run build` in `ui/` before any cargo or
docker step, and both carry a comment saying it is a prerequisite rather than a
convenience, because it reads like an optional frontend step and is not one.

The alternative fix — committing `ui/dist` — was rejected. Generated bundles in
git produce conflicts on every branch that touches the UI, and the failure mode
they'd prevent is loud and immediate rather than subtle.

There is a real asymmetry left here: a developer's working tree has `ui/dist`
from the last `npm run build`, so a stale UI compiles happily into a local
`fghjd`. Nothing checks freshness. That is accepted — the UI is served from the
binary, so the symptom is "my change didn't appear", which points at its own
cause.

## Why the binaries are built per OS and the sidecar once

Everything OS-specific sits behind `src/platform/`, and Linux (Ubuntu LTS) has
its own implementations there: systemd-resolved for split DNS, Debian's CA
store and the browsers' NSS databases for trust. What it doesn't have yet is a
package of its own, built by the release beside the macOS binaries; see [The
Debian package](#the-debian-package) below. The Homebrew formula says
`depends_on :macos`, because the tap is the macOS install path.

The sidecar is the opposite: it is a Linux container by definition, and it has
to match whatever architecture the user's Docker runs. The release ships
`darwin-arm64` only — Intel Macs were dropped, their runner being the slowest
job in the release — so in practice that is `linux/arm64`. The sidecar still
builds `linux/amd64` too, natively and cheaply, for the Linux port. So there
are two matrices in one workflow with nothing in common but the crate they
build from. [[in-network-sidecar]] covers how they're
merged into one manifest list and why they're built on native runners rather
than under QEMU.

## The Homebrew tap is a second repo, updated after the fact

`Formula/fghj.rb` lives in `mlnja/homebrew-tap`, not here, and pins a `sha256`
per platform. Those checksums cannot exist until the release assets do, so the
sequence is necessarily two-phase: `just release` → GitHub Actions publishes →
`just update-tap` reads each `.tar.gz.sha256` back off the release and rewrites
the formula.

That second phase **waits** rather than failing. The gap between the two is
not small — `create-release` is gated on two full release compiles of the
crate plus two more inside Docker for the sidecar — so `update-tap` run
straight after `release` used to 404 on its first `curl`, which reads as a
broken formula rather than as "not yet". It now polls the `.sha256` assets
until they appear (`TAP_WAIT_TIMEOUT`, default 2700s; `TAP_WAIT_INTERVAL`,
default 15s), which makes "release and walk away" the normal path. It polls
those assets rather than the release object or the workflow run because they
are the exact bytes the recipe goes on to read, so their presence is the real
precondition — and checking them needs neither `gh` nor a token.

The order of the two failure modes matters: the missing-tap-checkout check
runs *before* the poll, so a forgotten `git clone` of the tap fails in a
second instead of after the full timeout.

The formula installs `fghjd` as a **LaunchDaemon** with `require_root true`,
not a per-user LaunchAgent: it binds 80/443, installs a system-trusted root CA,
and edits `/etc/resolver` and `/etc/hosts`. `sudo brew services start fghj` is
the documented start, and the privilege split that makes that safe to run as
root is [[persistence-and-workspace-store]]'s subject.

## The Debian package

Built by the release's `deb` job from `[package.metadata.deb]` in
`Cargo.toml` and `packaging/debian/`. Ubuntu installs fghj
from `apt.mlops.ninja` (`mlnja/apt`, a signed static repository on GitHub
Pages), which rebuilds its index from each product's GitHub release assets.

- **Built** with `cargo-deb` on `ubuntu-22.04` and `ubuntu-22.04-arm`, the
  oldest supported LTS, so the glibc it links against is one every later
  LTS has. One `.deb` per architecture serves 22.04, 24.04 and 26.04.
- **Ships** `fghj`, `fghjd` and `fghjd.service`, with `Restart=always`: when
  `fghj wire` names a different Docker, fghjd exits so the service manager
  starts it on the new socket ([[docker-and-downloads]]). The package's
  scripts enable and (re)start the unit, so installing is the whole setup
  until the first `fghj wire`.
- **Removing** it runs `fghj uninstall --yes --keep-workspaces` from
  `prerm`, while the binary still exists: no CA of fghj's survives it, not
  trusted (system store, every workspace owner's browsers), not on disk
  (`ca/`, `certs/`, `sidecar-ca/`), not in a running sidecar. Only the
  workspace index and daemon settings stay, for a reinstall. `apt purge`
  deletes those and each workspace's `.fghj/fghj.db`, reading the paths
  from the index in shell, since the binary is gone by then.
- **Checked** by installing it into clean `ubuntu:22.04`, `24.04` and
  `26.04` containers in the same job, before anything is published.
- **Published** by attaching the `.deb`s to the GitHub release, then the
  `apt` job sending `mlnja/apt` a `repository_dispatch` of type `release`.
  That needs `APT_DISPATCH_TOKEN`, a `prod` environment secret (so only the
  jobs that say `environment: prod` see it), a fine-grained token with Contents:write
  on `mlnja/apt`; without it the job only warns, and `mlnja/apt`'s daily run
  picks the release up.

Its dependencies are everything `fghjd` runs:

| Package | Why | Field |
|---|---|---|
| `libnss3-tools` | `certutil`, which adds the CA to Firefox's and Chrome's own databases | Depends |
| `ca-certificates` | `update-ca-certificates`, for the system store | Depends |
| `systemd-resolved \| systemd (<< 251)` | split DNS through the `fghj0` link; its own package from 24.04, part of `systemd` 249 on 22.04 | Depends |
| `iproute2` | `ip`, to create the `fghj0` link | Depends |
| `git` | cloning the repos a workspace includes | Depends |
| `docker.io \| docker-ce \| docker-desktop` | the engine | Recommends |

`libnss3-tools` is a hard dependency although fghj runs without it: without
it, the one place the failure shows is the browser's certificate warning,
which the doctor catches but nobody should have to meet. It also brings
`libnss3`, which a machine whose only browser is the snap Firefox doesn't
have, since snaps bundle their own. Docker is only recommended, because users
install it in mutually exclusive ways (Ubuntu's, Docker's, Desktop, rootless)
and a hard dependency would pick one for them; apt installs recommends by
default, and the doctor covers the rest.

## What CI checks, and the one suite it doesn't run

`ci.yml` runs four jobs:

- **Check**, on `macos-latest`, `ubuntu-22.04`, `ubuntu-24.04` and
  `ubuntu-latest` (26.04): `cargo check --all-targets`, `clippy -D
  warnings`, `cargo fmt --check`, `cargo test`. Every platform backend
  compiles on every OS, so each run also tests the other OS's parsers; what
  only one OS can test is what probes the real system.
- **Debian package** (Linux): builds the `.deb` and runs
  `packaging/check-deb.sh` on it, the same check the release does.
- **Sidecar image** (Linux): builds `sidecar/Dockerfile` on one architecture
  without pushing. The sidecar's Dockerfile is embedded in `fghjd` and only
  exercised on a fallback path, so a break in it would otherwise stay invisible
  until it hit a user — and then only some users.
- **Schema** (Linux): `cue vet schema/*.cue`. The schema is an authoring aid
  rather than the enforcing boundary ([[config-language]]), but its one
  obligation is to never accept what the daemon rejects, and a schema that
  doesn't parse can't hold up its end. Deliberately *not* `cue fmt --check`:
  cue's formatter aligns values across adjacent lines, so one long field pads
  its neighbours into columns of whitespace and a comment edit reflows lines it
  didn't touch. Gating on that buys churn, not correctness.

The tests marked
`#[ignore = "needs a Docker daemon: ..."]`, two dozen of them, — the ones that start real
containers or run real builds (`docker/`'s exec and BuildKit tests,
`orchestrator/health.rs`'s `wait_for_exit` tests, `orchestrator/start_node.rs`'s task tests,
`orchestrator/observe.rs`'s published-port test, `persistence/rehydrate.rs`'s
dead-container test). They run with `cargo test -- --ignored` on any machine
with Docker.

GitHub's macOS runners cannot run Docker — no nested virtualization, so
Docker Desktop won't start. Its Ubuntu runners can, so that's where these
would run; they don't yet (see the known gaps). End to end, the machine
itself is checked by `e2e/files-example.sh` against the tutorial's example
app, run by hand on a Mac or an Ubuntu box with fghjd up.

`cargo test` with an unreachable `DOCKER_HOST` is the check that the split is
honest: if a test that needs a daemon is missing its `#[ignore]`, it fails
there instead of in CI.

Running that check turned up a second class of failure with nothing to do with
Docker. Eight tests — `orchestrator::registry`'s node-lock tests and
`daemon::registry`/`daemon::control`'s workspace-index tests — are pure
bookkeeping, but they sit behind types that *own* a `bollard::Docker`, and
`connect_with_local_defaults` resolves the unix socket at **construction**:
no socket, `SocketNotFoundError`, panic, before a single request is sent.
`docker::undialled_client()` builds an http-transport client pointed at port 1
instead. Nothing listens there, so a request that shouldn't happen still fails
loudly — it just no longer requires Docker to be installed in order for a test
about a mutex to run.

## Known gaps

- **The Docker-backed suite isn't in CI.** The Ubuntu Check jobs could run it
  with `-- --ignored`, since those runners have Docker; nobody has wired
  that up, and some of those tests may still assume macOS.
- **Homebrew can't clean up on its own.** `brew uninstall` runs unprivileged,
  so the trusted CA and `/var/lib/fghjd` outlive it unless `sudo fghj
  uninstall` runs first; the docs say so, but nothing enforces it. The
  `.deb` does it in `prerm`.

## Related

- [[in-network-sidecar]] — where the sidecar image comes from, in detail.
- [[persistence-and-workspace-store]] — the root-daemon/user-clone privilege
  split the LaunchDaemon install depends on.
- [[config-language]] — why `cue vet` in CI is a courtesy to authors and not a
  security boundary.
