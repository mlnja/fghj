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

- **macOS on Apple Silicon**, through [Homebrew](#homebrew). Intel Macs
  aren't supported.
- **Ubuntu 22.04 LTS or newer**, `amd64` or `arm64`, through [apt](#ubuntu).
  Other Linux distributions aren't packaged yet. Windows isn't supported.
- **Docker** — Docker Desktop, OrbStack, or Colima on macOS; Docker Engine,
  rootless Docker or Docker Desktop on Linux. `fghjd` uses the same one your
  `docker` command does; your first `fghj wire` tells it which.
- **[CUE](https://cuelang.org/docs/install/)** — only needed for
  `fghj validate`, which shells out to the `cue` CLI to check an
  `.fghj.yaml` against fghj's schema. Everything else works without it: the
  daemon's own Rust types are the enforcing boundary, and the schema is an
  authoring aid (see [`.fghj.yaml` reference](/reference/fghj-yaml/)).

:::note[What this changes on your machine]
`fghjd` edits `/etc/hosts` and `/etc/resolver`, aliases two loopback
ranges, and trusts a local root CA. Everything but the
CA and its state directory is undone by `fghj daemon stop`; those two are
removed by `sudo fghj uninstall`. The full list, with commands to check it
yourself, is [What fghj touches, and how to remove
it](/guides/uninstalling/).
:::

## Homebrew

```bash
brew install mlnja/tap/fghj
sudo brew services start fghj
```

The second command needs `sudo`. That is not an oversight:
`brew install` runs unprivileged and Homebrew never starts a service at
install time (`brew install postgresql` doesn't start Postgres either), and
`fghjd` is declared with `require_root true` because it binds ports 80/443
(privileged on macOS regardless of which address they're on), installs a
root CA, and edits `/etc/resolver` and `/etc/hosts`. Homebrew
therefore installs it as a *LaunchDaemon* under `/Library/LaunchDaemons`
rather than a per-user LaunchAgent, and refuses to start it unprivileged.
This is the same shape as `dnsmasq` or `nginx` on macOS.

On its first start `fghjd` generates the local root CA, writes
`/etc/resolver/fghj.internal`, and binds ports 80/443 on its own loopback
alias — see [Start the daemon](#start-the-daemon) below. Two things it can't
do on its own, and your first [`fghj wire`](/cli/wire/) does instead, since
nothing works before something is wired anyway:

- **Pick your Docker.** Root has no docker context of its own, so `wire`
  works out the one your `docker` command uses (`DOCKER_HOST`,
  `DOCKER_CONTEXT`, then your current context) and tells `fghjd`.
- **Get the CA trusted.** macOS asks for your password before trusting a
  root, and a LaunchDaemon has no login session to ask in. `wire` runs in
  your terminal, as you, so it can: the password dialog shows once, and
  later runs find the CA trusted and say nothing.

Then:

```bash
fghj daemon status      # -> fghjd is running and active
fghj doctor             # Docker and CA show as pending until your first wire
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

Docker containers, networks and volumes are *not* removed by either command
(removing each workspace from its Config tab first does that),
and neither are repositories fghj cloned into your workspace. For the
complete inventory of every file, system setting and Docker resource fghj
creates — and how to verify nothing is left — see [What fghj touches, and
how to remove it](/guides/uninstalling/).

## Ubuntu

```bash
sudo curl -fsSLo /etc/apt/keyrings/mlops-ninja.asc https://apt.mlops.ninja/key.asc
echo "deb [signed-by=/etc/apt/keyrings/mlops-ninja.asc] https://apt.mlops.ninja stable main" \
  | sudo tee /etc/apt/sources.list.d/mlops-ninja.list
sudo apt update
sudo apt install fghj
```

That's all of it. The package installs `fghj` and `fghjd` into `/usr/bin`
and runs `fghjd` as the `fghjd` systemd unit, enabled and started, and
restarted on every upgrade. `sudo apt upgrade` keeps it current, like
everything else on the machine. Each release is also on the [releases
page](https://github.com/mlnja/fghj/releases) as a `.deb`, for
`sudo apt install ./fghj_<version>_<arch>.deb`.

On Linux nothing needs your password after that. `fghjd` runs as root, so on
every start it puts fghj's root CA into the system store
(`/usr/local/share/ca-certificates`, for curl, Python, Go and the rest) and
routes `*.fghj.internal` through systemd-resolved. Browsers keep their own
certificate stores, so it also adds the CA to the Firefox profiles and Chrome
database in each workspace owner's home, running `certutil` as that user so
the files stay theirs. Your first `fghj wire` does that for you before it
returns; restart a browser that was already open.

```bash
systemctl status fghjd   # running
fghj doctor              # Docker and the CA show as pending until your first wire
journalctl -u fghjd      # its log
```

### Uninstalling

```bash
sudo apt remove fghj     # stops fghjd, and takes the CA out of every store
sudo apt purge fghj      # also deletes /var/lib/fghjd: workspaces, CA keys, run state
```

Removing the package runs `fghj uninstall` before the binaries go, so no
trusted root outlives the software holding its key. As with Homebrew,
Docker resources and the repositories fghj cloned stay; see [What fghj
touches, and how to remove it](/guides/uninstalling/).

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

The first time it starts it generates a local root CA, trusts it in the
System keychain, and writes `/etc/resolver/fghj.internal` so macOS sends
`*.fghj.internal` lookups to `fghjd`'s own DNS server. Those are the two
system-level changes fghj makes; both are idempotent on later starts.

For a persistent install, supervise it with a launchd `LaunchDaemon` (a
*daemon*, not a per-user `LaunchAgent` — it has to be root). The Homebrew
formula declares exactly that, which is why it's started with
`sudo brew services start fghj` rather than the usual unprivileged form.
Under launchd, `fghjd` can't show the password dialog that trusting the CA
needs, so it logs that and carries on; your first `fghj wire` trusts it from
your terminal.

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
they toggle whether the running daemon is occupying the machine, and `start`
and `restart` also trust the CA if it isn't trusted yet. Stopping or
restarting the process itself is your supervisor's job
(`sudo brew services stop fghj`, `sudo brew services restart fghj`, or
Ctrl-C).

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

- [The tutorial](/tutorial/) — run a four-repo example app, one
  chapter at a time. Start here if you want to understand fghj.
- [Quickstart](/getting-started/quickstart/) — the short version, if you
  already know what you're doing.
