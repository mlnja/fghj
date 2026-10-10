---
title: Installation
description: How to install fghj and fghjd, and how to start the daemon.
---

`fghj` is two binaries: `fghj`, the unprivileged CLI you run day to day, and
`fghjd`, the root-owned superdaemon that serves ports 80/443, the
`*.fghj.internal` DNS zone, the local CA, and the control API. Both come from
the same build; they must always be the same version, because the CLI speaks
a private HTTP protocol over `fghjd`'s Unix socket with no compatibility
shims.

## Prerequisites

- **macOS on Apple Silicon.** Intel Macs aren't supported. **Linux support is in progress.**
  Four pieces of `fghjd` are written against macOS interfaces and each needs a
  Linux counterpart: its split-DNS integration (writing
  `/etc/resolver/fghj.internal`), its trust-store handling (shelling out to
  `security`), its raw-port NAT (`pf`), and its SSH-agent-socket recovery for
  Git-over-SSH clones. Until those land there is no Linux build to install —
  the release publishes `darwin-arm64` only. Windows would
  need the same work again and nobody is doing it.
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
`fghjd` is declared with `require_root true` because it binds ports 80/443
(privileged on macOS regardless of which address they're on), installs a
root CA, and edits `/etc/resolver` and `/etc/hosts`. Homebrew
therefore installs it as a *LaunchDaemon* under `/Library/LaunchDaemons`
rather than a per-user LaunchAgent, and refuses to start it unprivileged.
This is the same shape as `dnsmasq` or `nginx` on macOS.

Everything else is automatic. On its first start `fghjd` generates the local
root CA, installs it into the System keychain, writes
`/etc/resolver/fghj.internal`, and binds ports 80/443 on its own loopback
alias — see [Start the daemon](#start-the-daemon) below. Then:

```bash
fghj daemon status      # -> fghjd is running and active
```

### If you already use ports 80 or 443

You can still install and run fghj. `fghjd` binds 80/443 on `127.222.0.1`,
a loopback address it aliases onto `lo0` for itself, and never on
`127.0.0.1` or `0.0.0.0`. A local nginx, Apache, Caddy or
`docker run -p 80:80` keeps serving, and `curl http://127.0.0.1/` still
reaches it — fghj is only ever reached through its `*.fghj.internal` names,
which resolve to its own address.

The address lives in `127.0.0.0/8` on purpose: RFC 1122 forbids a
loopback-destined packet from reaching the wire, so unlike a `10.x` address
it can never collide with your LAN, VPN or office network.

The one case that still conflicts is a server that binds `0.0.0.0:80`
*without* `SO_REUSEADDR`. That's rare — nginx, Apache, Caddy, Docker, Go and
Node all set it as standard practice — but such a server would already fail
against anything else holding port 80 on the machine.

### Uninstalling

```bash
sudo fghj uninstall
brew uninstall fghj
```

`brew uninstall` runs unprivileged, so it cannot touch the two artifacts
that outlive it: the root CA trusted in your System keychain, and `fghjd`'s
durable state under `/var/lib/fghjd` — which holds that CA's private key.
`sudo fghj uninstall` removes both, along with every system change the
daemon made. It lists what it will delete and asks first; `-y` skips the
prompt, and `--keep-ca` / `--keep-state` leave either behind if you are
reinstalling rather than leaving.

Docker containers, networks and volumes are *not* removed by either command,
and neither are repositories fghj cloned into your workspace. For the
complete inventory of every file, system setting and Docker resource fghj
creates — and how to verify nothing is left — see [What fghj touches, and
how to remove it](/guides/uninstalling/).

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

`fghjd` needs root to bind ports 80/443, alias its loopback address, answer
DNS, install its CA into the system trust store, and edit `/etc/hosts`. It
does not daemonize itself — it expects to be supervised, or run in the
foreground:

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
  ports 80/443, DNS, its loopback alias, and its `/etc/hosts` block, and
  isn't reconciling. This is
  what `fghj daemon stop` leaves behind, so you can hand those ports to
  something else for a while. `fghj daemon start` takes them back.
- **`fghjd is running and active`** — the normal state.

`fghj daemon start`/`stop`/`restart` never touch the *process* lifecycle;
they toggle whether the running daemon is occupying the machine. Stopping
the process itself is your supervisor's job (`sudo brew services stop fghj`,
or Ctrl-C).

For the full picture — Docker and BuildKit, the loopback alias, both ports,
OS DNS routing and CA trust, each checked against the machine rather than
against what `fghjd` thinks it set up — run [`fghj doctor`](/cli/doctor/):

```bash
$ fghj doctor
✓ fghjd control socket: connected to /var/run/fghjd.sock
✓ Docker Engine reachable: server 29.4.0, API 1.54
✓ BuildKit available: API 1.54 supports the BuildKit session endpoint
✓ fghjd is active: serving DNS, 80 and 443
...
all clear
```

It's the right first command for any "nothing resolves" or "connection
refused" problem, since all of those failures look identical from the
browser.

## Next

- [The tutorial](/tutorial/) — build a two-repo workspace from nothing, one
  chapter at a time. Start here if you want to understand fghj.
- [Quickstart](/getting-started/quickstart/) — the short version, if you
  already know what you're doing.
