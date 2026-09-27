# Release and delivery: what "installed fghj" actually means

> **Status:** implemented. `Justfile` (`release`, `update-tap`),
> `.github/workflows/ci.yml`, `.github/workflows/release.yml`,
> `src/sidecar_image.rs`, and `Formula/fghj.rb` in the separate
> `mlnja/homebrew-tap` repo. The known gaps are listed at the bottom and
> tracked in [[AUDIT]].

Every other file in this folder describes how fghj behaves once it is running.
This one describes how it gets onto a machine, because that turned out to have
its own set of non-obvious constraints — and one of them was quietly broken.

## Four artifacts, one version

An installed fghj is not one thing:

| Artifact | Where it comes from | Where it goes |
|---|---|---|
| `fghj` (CLI) | `release.yml`'s macOS matrix | `bin/` via Homebrew |
| `fghjd` (root daemon) | same | `bin/`, plus a LaunchDaemon |
| `ghcr.io/mlnja/fghj-sidecar` | `release.yml`'s linux matrix | the user's Docker image store |
| the UI | compiled *into* both binaries | nowhere — see below |

All four are keyed to one number: the `version` in `Cargo.toml`. The CLI and
daemon carry it as `CARGO_PKG_VERSION`; the sidecar image is tagged with it
(see [[in-network-sidecar]] for why the reference is registry-qualified); the
Homebrew formula pins it. `just release` bumps it, commits, tags, and pushes,
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

## Why the binaries are macOS-only and the sidecar is not

`fghjd` shells out to macOS's `security` CLI for trust-store management and
writes macOS-specific paths (`/etc/resolver`, the Keychain) — see
[[local-ca-and-tls-proxy]] and [[split-dns]]. The Homebrew formula says
`depends_on :macos` and offers no source-build fallback, because building from
source wouldn't run anywhere else either.

The sidecar is the opposite: it is a Linux container by definition, and it has
to match whatever architecture the user's Docker runs. On an Apple Silicon Mac
that is `linux/arm64`; on an Intel Mac, `linux/amd64`. Since the release ships
`darwin-arm64` and `darwin-amd64` assets, the sidecar needs both Linux
architectures — so there are two matrices in one workflow with nothing in
common but the crate they build from. [[in-network-sidecar]] covers how they're
merged into one manifest list and why they're built on native runners rather
than under QEMU.

## The Homebrew tap is a second repo, updated after the fact

`Formula/fghj.rb` lives in `mlnja/homebrew-tap`, not here, and pins a `sha256`
per platform. Those checksums cannot exist until the release assets do, so the
sequence is necessarily two-phase: `just release` → GitHub Actions publishes →
`just update-tap` reads each `.tar.gz.sha256` back off the release and rewrites
the formula.

The formula installs `fghjd` as a **LaunchDaemon** with `require_root true`,
not a per-user LaunchAgent: it binds 80/443, installs a system-trusted root CA,
and edits `/etc/resolver` and `/etc/hosts`. `sudo brew services start fghj` is
the documented start, and the privilege split that makes that safe to run as
root is [[persistence-and-workspace-store]]'s subject.

## What CI checks, and the one suite it can't run

`ci.yml` runs three jobs:

- **Check** (macOS): `cargo check --all-targets`, `clippy -D warnings`,
  `cargo fmt --check`, `cargo test`.
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

Fourteen tests are marked
`#[ignore = "needs a Docker daemon: ..."]` — the ones that start real
containers or run real builds (`docker.rs`'s exec and BuildKit tests,
`runs/health.rs`'s `wait_for_exit` tests, `runs/start_node.rs`'s task tests,
`runs/observe.rs`'s published-port test, `persistence/rehydrate.rs`'s
dead-container test). They run with `cargo test -- --ignored` on any machine
with Docker, which in practice means a developer's Mac.

They are not in CI because GitHub's macOS runners cannot run Docker — no
nested virtualization, so Docker Desktop won't start. The check job is macOS
because that's the only platform the daemon's own code paths are written for,
and those two facts are in direct conflict.

`cargo test` with an unreachable `DOCKER_HOST` is the check that the split is
honest: if a test that needs a daemon is missing its `#[ignore]`, it fails
there instead of in CI.

Running that check turned up a second class of failure with nothing to do with
Docker. Eight tests — `runs::registry`'s node-lock tests and
`daemon::registry`/`daemon::control`'s workspace-index tests — are pure
bookkeeping, but they sit behind types that *own* a `bollard::Docker`, and
`connect_with_local_defaults` resolves the unix socket at **construction**:
no socket, `SocketNotFoundError`, panic, before a single request is sent.
`docker::undialled_client()` builds an http-transport client pointed at port 1
instead. Nothing listens there, so a request that shouldn't happen still fails
loudly — it just no longer requires Docker to be installed in order for a test
about a mutex to run.

## Known gaps

- **The Docker-backed suite is developer-only.** The lib is known to compile on
  Linux (the sidecar image is proof — it builds the whole crate on
  `rust:1-bookworm`), so a Linux test job with Docker available is the obvious
  next move. The tests have never been run there, and some may carry macOS
  assumptions, so this is a real piece of work rather than a config line.
- **No `LICENSE` file** and no `license` field in `Cargo.toml`. The tap formula
  says `license "MIT"` with a `TODO: confirm` beside it.
- **No uninstall path.** The CA stays in the system Keychain and the resolver
  file under `/etc/resolver` across a full daemon stop, deliberately (see
  `PROGRESS.md`), but `brew uninstall` leaves both behind with nothing to
  clean them up.

## Related

- [[in-network-sidecar]] — where the sidecar image comes from, in detail.
- [[persistence-and-workspace-store]] — the root-daemon/user-clone privilege
  split the LaunchDaemon install depends on.
- [[config-language]] — why `cue vet` in CI is a courtesy to authors and not a
  security boundary.
