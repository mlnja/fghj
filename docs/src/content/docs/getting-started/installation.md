---
title: Installation
description: How to install fghj and fghjd, and how to start the daemon.
---

`fghj` is two binaries: `fghj`, the unprivileged CLI you run day to day, and
`fghjd`, the root-owned superdaemon that owns ports 80/443, the
`*.fghj.internal` DNS zone, the local CA, and the control API. Both come from
the same build; they must always be the same version, because the CLI speaks
a private HTTP protocol over `fghjd`'s Unix socket with no compatibility
shims.

## Prerequisites

- **macOS.** `fghjd`'s split-DNS integration (writing
  `/etc/resolver/fghj.internal`), its trust-store handling (shelling out to
  `security`), and its SSH-agent-socket recovery for Git-over-SSH clones are
  macOS-specific. Linux and Windows would each need equivalent mechanisms
  added; nothing stubs them out today.
- **Docker** — Docker Desktop, OrbStack, or Colima. `fghjd` connects to
  whichever Docker context is currently active and refuses to start if it
  can't reach it.
- **[CUE](https://cuelang.org/docs/install/)** — only needed for
  `fghj validate`, which shells out to the `cue` CLI to check an
  `.fghj.yaml` against fghj's schema. Everything else works without it: the
  daemon's own Rust types are the enforcing boundary, and the schema is an
  authoring aid (see [`.fghj.yaml` reference](/reference/fghj-yaml/)).

## Homebrew

There is a tap, `mlops-ninja/homebrew-tap`:

```bash
brew install mlops-ninja/tap/fghj
```

:::caution[Not usable yet]
The formula points at release tarballs — and no release has been tagged yet,
so the download URLs 404 and the checksums in the formula are still
placeholders. Until the first `v0.1.0` release is published, **build from
source** as below. The formula is in the repo's sibling tap so that the
release pipeline has somewhere to publish to, not because the path works
today.
:::

## Build from source

You need **Rust** (stable — install via [rustup](https://rustup.rs)) and
**Node 22+**, because the Svelte UI is embedded into the `fghjd` binary at
compile time with `include_dir!`. That means the UI has to be built *first* —
a bare `cargo build` in a fresh clone embeds whatever is (not) in `ui/dist`:

```bash
git clone https://github.com/mlnja/fghj.git
cd fghj

# 1. The UI — a compile-time input to the daemon, not a runtime asset
cd ui && npm install && npm run build && cd ..

# 2. The binaries
cargo build --release
```

This produces three binaries under `target/release/`:

| Binary | What it is |
|---|---|
| `fghj` | The CLI. Runs as you, talks to `fghjd` over its Unix socket. |
| `fghjd` | The superdaemon. Runs as root: DNS server, TLS proxy, local CA, control API, reconciler. |
| `fghj-sidecar` | The in-network DNS + TLS proxy that runs one container per run. You never invoke it or put it on `PATH` — `fghjd` bakes it into a Docker image on demand. See [In-network sidecar](/concepts/sidecar/). |

Put the first two on your `PATH`:

```bash
sudo cp target/release/fghj target/release/fghjd /usr/local/bin/
```

## Start the daemon

`fghjd` needs root to bind 80/443, answer DNS, install its CA into the
system trust store, and edit `/etc/hosts`. It does not daemonize itself — it
expects to be supervised, or run in the foreground:

```bash
sudo fghjd
```

The first time it starts it generates a local root CA, installs it into your
system trust store, and writes `/etc/resolver/fghj.internal` so macOS sends
`*.fghj.internal` lookups to `fghjd`'s own DNS server. Those are the two
system-level changes fghj makes; both are idempotent on later starts.

For a persistent install, supervise it with a launchd `LaunchDaemon` (a
*daemon*, not a per-user `LaunchAgent` — it has to be root). The Homebrew
formula declares exactly that, which is why it's started with
`sudo brew services start fghj` rather than the usual unprivileged form.

## Verify

```bash
$ fghj daemon status
fghjd is running and active
```

Three states are worth telling apart:

- **`fghjd is not running`** — nothing is listening on the control socket.
  Start the process.
- **`fghjd is running and idle`** — the process is up but has released
  80/443, DNS, and its `/etc/hosts` block, and isn't reconciling. This is
  what `fghj daemon stop` leaves behind, so you can hand those ports to
  something else for a while. `fghj daemon start` takes them back.
- **`fghjd is running and active`** — the normal state.

`fghj daemon start`/`stop`/`restart` never touch the *process* lifecycle;
they toggle whether the running daemon is occupying the machine. Stopping
the process itself is your supervisor's job (`sudo brew services stop fghj`,
or Ctrl-C).

## Next

- [The tutorial](/tutorial/) — build a two-repo workspace from nothing, one
  chapter at a time. Start here if you want to understand fghj.
- [Quickstart](/getting-started/quickstart/) — the short version, if you
  already know what you're doing.
