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

```bash
brew install mlnja/tap/fghj
sudo brew services start fghj
```

Two commands, and the second one needs `sudo`. That is not an oversight:
`brew install` runs unprivileged and Homebrew never starts a service at
install time (`brew install postgresql` doesn't start Postgres either), and
`fghjd` is declared with `require_root true` because it binds 80/443,
installs a root CA, and edits `/etc/resolver` and `/etc/hosts`. Homebrew
therefore installs it as a *LaunchDaemon* under `/Library/LaunchDaemons`
rather than a per-user LaunchAgent, and refuses to start it unprivileged.
This is the same shape as `dnsmasq` or `nginx` on macOS.

Everything else is automatic. On its first start `fghjd` generates the local
root CA, installs it into the System keychain, writes
`/etc/resolver/fghj.internal`, and takes ports 80/443 — see
[Start the daemon](#start-the-daemon) below. Then:

```bash
fghj daemon status      # -> fghjd is running and active
```

:::caution[Needs a tagged release]
The formula points at release tarballs, and no release has been tagged yet,
so the download URLs 404 and the checksums in the formula are still
placeholders. Until the first `v0.1.0` release is published, **build from
source** as below. Once a tag is pushed, the release workflow's `tap` job
rewrites the formula with real checksums automatically.
:::

### Uninstalling

```bash
sudo brew services stop fghj      # unwinds /etc/resolver and /etc/hosts
brew uninstall fghj
```

Two things `brew uninstall` cannot remove, because it runs unprivileged —
the root CA in your System keychain, and `fghjd`'s durable state:

```bash
sudo security delete-certificate -c "fghj local CA" \
  /Library/Keychains/System.keychain
sudo rm -rf /var/lib/fghjd
```

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
