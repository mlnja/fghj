# fghj

[![Release](https://img.shields.io/github/v/release/mlnja/fghj?sort=semver)](https://github.com/mlnja/fghj/releases/latest)
[![CI](https://github.com/mlnja/fghj/actions/workflows/ci.yml/badge.svg)](https://github.com/mlnja/fghj/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platform: macOS arm64 | Ubuntu](https://img.shields.io/badge/platform-macOS%20arm64%20%7C%20Ubuntu-lightgrey)

**Local dev environments scoped to a user flow, not your whole fleet.**

[Docs](https://fghj.mlops.ninja/) ·
[Tutorial](https://fghj.mlops.ninja/tutorial/) ·
[What it fixes](https://fghj.mlops.ninja/guides/what-it-fixes/) ·
[What it changes on your machine](https://fghj.mlops.ninja/guides/uninstalling/)

Testing one user journey shouldn't mean booting every microservice you own.
`fghj` reads a small `.fghj.yaml` in each repo, follows the dependencies
outward from whichever repos you're actually working on, and brings up just
that subgraph — with real `*.fghj.internal` HTTPS domains, locally trusted
certificates, and no `docker-compose.yml` to hand-maintain.

> **Status: early.** macOS on Apple Silicon and Ubuntu 22.04+ (`amd64`,
> `arm64`). Released through the Homebrew tap and apt.mlops.ninja; building
> from source (below) also works. The config language is settled enough to write
> against; the `version: "2.0"` field in `.fghj.yaml` is the compatibility
> hook.

## The one idea

No repo owns the graph. Every repo declares only what *it* needs, in its own
file, and any repo can be the entry point:

```yaml
# storefront/.fghj.yaml
version: "2.0"

include:
  catalog: https://github.com/you/catalog.git

services:
  web:
    build: .
    ports:
      "3000":
        primary: true
    environment:
      DATABASE_URL: postgres://shop:dev@${FGHJ_SERVICE_FQDN:db}:5432/shop
      CATALOG_URL: http://${FGHJ_SERVICE_FQDN:catalog/api}:4000
    depends_on: [db, catalog]
  db:
    image: postgres:16
    ports: ["5432"]
```

If you know Docker Compose, you can read it already. `include:` is the one
addition: it names another repo, and `depends_on: catalog` waits on it.
That's a graph across two repos. `web.storefront` answers at
`https://web.storefront.<workspace>.fghj.internal` with a certificate your
system already trusts; its Postgres at
`db.storefront.<workspace>.fghj.raw.internal:5432`. Neither address is
written down anywhere — both are derived from the node's identity, which is
why the same config works unchanged in a second, isolated run.

When the whole fleet is too much to boot, a repo publishes **flows** — named
start lists, like `checkout: [web, catalog/browse]` — and you start one of
those instead.

## Install

On Ubuntu 22.04 or newer, from [apt.mlops.ninja](https://apt.mlops.ninja/),
which also keeps it updated:

```bash
sudo curl -fsSLo /etc/apt/keyrings/mlops-ninja.asc https://apt.mlops.ninja/key.asc
echo "deb [signed-by=/etc/apt/keyrings/mlops-ninja.asc] https://apt.mlops.ninja stable main" \
  | sudo tee /etc/apt/sources.list.d/mlops-ninja.list
sudo apt update && sudo apt install fghj
fghj doctor           # checks Docker, ports, DNS and the CA against the machine
```

The package runs `fghjd` as a systemd unit and needs nothing else from you:
as root it puts fghj's CA into the system store and routes
`*.fghj.internal` through systemd-resolved, and your first `fghj wire` adds
the CA to your browsers. `sudo apt remove fghj` leaves no trace of the CA.

Via Homebrew (macOS on Apple Silicon):

```bash
brew install mlnja/tap/fghj
sudo brew services start fghj
fghj doctor           # checks Docker, ports, DNS and the CA against the machine
```

The `sudo` is real: `fghjd` binds ports 80/443, installs a root CA into your
system trust store, and edits `/etc/resolver` and `/etc/hosts`, so Homebrew
installs it as a root LaunchDaemon — the same shape as `dnsmasq` or `nginx`.
(No formula auto-starts a service; `brew install postgresql` doesn't either.)
Everything else is automatic. Your first `fghj wire` finishes setup: it
points `fghjd` at the same Docker your `docker` command uses, and asks macOS
to trust fghj's local CA, which shows a password dialog once. Until then
`fghj doctor` shows those two as pending, not broken.

It binds those ports on `127.222.0.1`, a loopback address of its own (macOS
needs it aliased; Linux already routes all of `127/8` to loopback) —
**not** on `127.0.0.1` or `0.0.0.0`. If you already run something on
port 80, a local nginx or a `docker run -p 80:80`, it keeps working and
`curl http://127.0.0.1/` still reaches it rather than fghj.

### Building from source

If you'd rather not use a package, you need Rust (stable), Node 22+, Docker,
and a [supported OS](https://fghj.mlops.ninja/getting-started/installation/#prerequisites).
`cue` is optional — only `fghj validate` needs it.

```bash
git clone https://github.com/mlnja/fghj.git
cd fghj

# The Svelte UI is embedded into the daemon at compile time — build it first
cd ui && npm install && npm run build && cd ..

cargo build --release
sudo cp target/release/fghj target/release/fghjd /usr/local/bin/
```

Then start the daemon. It needs root to bind ports 80/443, answer DNS,
install its root CA into the system trust store, and edit `/etc/hosts`:

```bash
sudo fghjd            # foreground; for real use, launchd on macOS, systemd on Linux
fghj daemon status    # -> "fghjd is running and active"
fghj doctor           # Docker, BuildKit, ports, DNS, CA — all checked against the machine
```

Full details: [Installation](https://fghj.mlops.ninja/getting-started/installation/).

### What it changes on your machine

A root daemon that edits system config should say exactly what it touches.
This is all of it:

| Change | macOS | Linux | Undone by |
|---|---|---|---|
| A managed block of host entries | `/etc/hosts`, between `# fghj-managed-begin`/`-end` | the same | `fghj daemon stop` |
| Split DNS for fghj's zones only | `/etc/resolver/fghj.internal` | the `fghj0` link in systemd-resolved | `fghj daemon stop` |
| Loopback aliases | `127.222.0.1` and `127.223.0.0/16` on `lo0` | none needed | `fghj daemon stop` |
| A local root CA, trusted for TLS | the System keychain, as `fghj local CA` | the system store, and each workspace owner's browser databases | `sudo fghj uninstall` |
| Daemon state, including the CA's private keys (root-only) | `/var/lib/fghjd` | the same | `sudo fghj uninstall` |
| fghj's database for each workspace | `<workspace>/.fghj/fghj.db` | the same | `sudo fghj uninstall` |
| Containers, networks and volumes, all prefixed `fghj-` | Docker | the same | only fghj's sidecars are removed; the rest is left alone, on purpose |

On Ubuntu, removing the package runs `fghj uninstall` for you.

Every leaf certificate is issued by an intermediate that is name-constrained
to reserved local TLDs (`.internal`, `.test`, `.local`, `.localhost`) and
excluded from IP addresses, so even a leaked signing key can't mint a cert
for a public site. The root's key never leaves `/var/lib/fghjd/ca/`.
Outside your workspaces, nothing in your home directory changes, except on
Linux the CA entry in your browsers' certificate databases. Inside a
workspace fghj writes only `.fghj/` and the repos it clones.
`sudo fghj uninstall` lists what it will remove and asks first. The full
inventory, with commands to verify nothing is left, is
[What fghj touches, and how to remove it](https://fghj.mlops.ninja/guides/uninstalling/).

## Use it

```bash
fghj validate ./.fghj.yaml          # check a config against the schema
fghj graph .                        # resolve the whole graph, print JSON
fghj wire .                         # register the workspace with the daemon
fghj exec db.storefront -- psql -U shop shop
fghj daemon {start,stop,restart,status}
fghj doctor                         # is anything fghj needs from this host missing?
```

Runs are started from the UI at `https://fghj.internal/`. `fghj wire` prints
the link.

## The pieces

| Binary | What it is |
|---|---|
| `fghj` | The CLI. Runs as you; talks to `fghjd` over a Unix socket. |
| `fghjd` | The superdaemon. Runs as root: authoritative DNS for `*.fghj.internal`, TLS-terminating proxy, local CA, control API, reconciler, and the embedded web UI. |
| `fghj-sidecar` | One container per run: DNS + TLS proxy *inside* the run's network, so `fghj.internal` names resolve the same way from a container as from the host. You never invoke it. |

Two domain zones, and the difference matters:

- **`*.fghj.internal`** — goes through the proxy. TLS terminated with a
  locally trusted cert, dispatched by SNI/Host. This is what a browser uses.
- **`*.fghj.raw.internal`** — the container's real IP, any protocol, no
  proxy. This is what one service uses to reach another's Postgres, Redis,
  or gRPC port.

## Documentation

The docs are at **[fghj.mlops.ninja](https://fghj.mlops.ninja/)**.

- **[The tutorial](https://fghj.mlops.ninja/tutorial/)** — build a two-repo
  workspace from nothing across eight chapters, ending with capturing
  the traffic between services. Start here.
- **[What it fixes](https://fghj.mlops.ninja/guides/what-it-fixes/)** and
  **[fghj and OrbStack](https://fghj.mlops.ninja/guides/vs-orbstack/)** —
  where it fits next to what you already use.
- **[`.fghj.yaml` reference](https://fghj.mlops.ninja/reference/fghj-yaml/)** —
  every field.
- **[Concepts](https://fghj.mlops.ninja/concepts/architecture/)** — how it
  works inside.

The source is the Astro Starlight site in `docs/`. To run it locally:

```bash
cd docs && npm install && npm run dev
```

## Repo layout

| Path | What's in it |
|---|---|
| `src/` | The Rust crate — CLI, daemon, resolver, effects, state machine. |
| `schema/*.cue` | CUE schemas for `.fghj.yaml`. An **authoring aid** (editors, CI, `fghj validate`) — the Rust serde types are the enforcing boundary. |
| `ui/` | The Svelte 5 UI, embedded into `fghjd` at compile time. |
| `sidecar/` | Dockerfile for the in-network sidecar image. |
| `packaging/` | The Debian package (`debian/`) and the check that installing and removing it leaves nothing behind. |
| `e2e/` | An end-to-end check against the tutorial's example app. |
| `docs/` | The documentation site. |
| `concepts/` | The durable design record — one file per subsystem or precept, plus `AUDIT.md`, the punch list. Read this before proposing an architecture change. |
| `SPEC.md` | Product vision (aspirational in places). |

## Contributing

Bug reports from real multi-repo setups are the most useful thing right now.
See [CONTRIBUTING.md](CONTRIBUTING.md) for how to build, the checks CI runs,
and where design decisions are recorded.

## License

MIT — see [LICENSE](LICENSE).
