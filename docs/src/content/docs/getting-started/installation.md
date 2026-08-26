---
title: Installation
description: Prerequisites and how to build fghj and fghjd from source.
---

`fghj` isn't packaged yet — you build it from source with Cargo. There's no
installer, no Homebrew tap, and no prebuilt binaries at this stage.

## Prerequisites

- **Rust** (stable) — install via [rustup](https://rustup.rs).
- **Docker** — Docker Desktop, OrbStack, or Colima. `fghjd` connects to
  whichever Docker context is currently active and refuses to start if it
  can't reach it.
- **[CUE](https://cuelang.org/docs/install/)** — only needed for
  `fghj validate`, which shells out to the `cue` CLI to check an
  `fghj.yaml` against fghj's schema.
- **macOS** — `fghjd`'s split-DNS integration (writing
  `/etc/resolver/fghj.internal`) and its SSH-agent-socket recovery for
  Git-over-SSH clones are currently macOS-specific. Linux/Windows support
  would need equivalent mechanisms added.

## Build

```bash
git clone https://github.com/virviil/fghj.git
cd fghj
cargo build --release
```

This produces two binaries under `target/release/`:

- **`fghj`** — the unprivileged CLI you run day to day.
- **`fghjd`** — the root-owned superdaemon (DNS server, TLS proxy, local
  CA, control API).

Put both on your `PATH`, e.g.:

```bash
cp target/release/fghj target/release/fghjd /usr/local/bin/
```

## The web UI

The Svelte UI lives under `ui/` and is embedded into the `fghjd` binary at
compile time (`include_dir!`), so it has to be built *before* `cargo build`
picks it up:

```bash
cd ui
npm install
npm run build
cd ..
cargo build --release
```

## Start the daemon

`fghjd` needs root to bind ports 80/443, run the DNS server, and install
its CA into the system trust store:

```bash
sudo fghjd
```

Leave it running in the foreground (or supervise it with `systemd`/a
`launchd` `LaunchDaemon` for a persistent install — `fghjd` doesn't
daemonize itself). The first time it starts, it generates a local CA,
installs it into your system's trust store, and writes
`/etc/resolver/fghj.internal` so macOS routes `*.fghj.internal` lookups to
its own DNS server.

Once it's up, verify the CLI can reach it:

```bash
fghj wire --help
```

Next: [Quickstart](/getting-started/quickstart/).
