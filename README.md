# fghj

**Local dev environments scoped to a user flow, not your whole fleet.**

Testing one user journey shouldn't mean booting every microservice you own.
`fghj` reads a small `.fghj.yaml` in each repo, follows the dependencies
outward from whichever repos you're actually working on, and brings up just
that subgraph — with real `*.fghj.internal` HTTPS domains, locally trusted
certificates, and no `docker-compose.yml` to hand-maintain.

> **Status: pre-release.** macOS only, no tagged release yet. Build from
> source (below). The config language is settled enough to write against;
> the `version: "1.0"` field in `.fghj.yaml` is the compatibility hook.

## The one idea

No repo owns the graph. Every repo declares only what *it* needs, in its own
file, and any repo can be the entry point:

```yaml
# storefront/.fghj.yaml
version: "1.0"

services:
  web:
    build:
      context: .
    ports:
      "3000":
        primary: true
    environment:
      DATABASE_URL: postgres://shop:dev@${FGHJ_SERVICE_FQDN:db}:5432/shop
      CATALOG_URL: http://${FGHJ_SERVICE_FQDN:api}:4000
    dependencies:
      - kind: backing
        name: db
        image: postgres:16
        ports: ["5432"]
      - kind: service
        repo: https://github.com/you/catalog.git
```

That's a four-node graph across two repos. `web.storefront` answers at
`https://web.storefront.<workspace>.fghj.internal` with a certificate your
system already trusts; its Postgres at
`db.web.storefront.<workspace>.fghj.raw.internal:5432`. Neither address is
written down anywhere — both are derived from the node's identity, which is
why the same config works unchanged in a second, isolated run.

## Install

No release is tagged yet, so build from source. You need Rust (stable),
Node 22+, Docker, and macOS. `cue` is optional — only `fghj validate`
needs it.

```bash
git clone https://github.com/mlnja/fghj.git
cd fghj

# The Svelte UI is embedded into the daemon at compile time — build it first
cd ui && npm install && npm run build && cd ..

cargo build --release
sudo cp target/release/fghj target/release/fghjd /usr/local/bin/
```

Then start the daemon. It needs root to bind 80/443, answer DNS, install its
root CA into the system trust store, and edit `/etc/hosts`:

```bash
sudo fghjd            # foreground; supervise with a launchd LaunchDaemon for real use
fghj daemon status    # -> "fghjd is running and active"
```

Full details, including the (not-yet-working) Homebrew path:
[`docs/.../installation.md`](docs/src/content/docs/getting-started/installation.md).

## Use it

```bash
fghj validate ./.fghj.yaml          # check a config against the schema
fghj graph .                        # resolve the whole graph, print JSON
fghj wire .                         # register the workspace with the daemon
fghj exec db.web.storefront -- psql -U shop shop
fghj daemon {start,stop,restart,status}
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

The docs live in `docs/` as an Astro Starlight site. It isn't deployed
anywhere yet, so read it either as plain Markdown in the repo or locally:

```bash
cd docs && npm install && npm run dev
```

- **[The tutorial](docs/src/content/docs/tutorial/)** — build a two-repo
  workspace from nothing across seven chapters. Start here.
- **[`.fghj.yaml` reference](docs/src/content/docs/reference/fghj-yaml.md)** —
  every field.
- **[Concepts](docs/src/content/docs/concepts/)** — how it works inside.

## Repo layout

| Path | What's in it |
|---|---|
| `src/` | The Rust crate — CLI, daemon, resolver, effects, state machine. |
| `schema/*.cue` | CUE schemas for `.fghj.yaml`. An **authoring aid** (editors, CI, `fghj validate`) — the Rust serde types are the enforcing boundary. |
| `ui/` | The Svelte 5 UI, embedded into `fghjd` at compile time. |
| `sidecar/` | Dockerfile for the in-network sidecar image. |
| `docs/` | The documentation site. |
| `concepts/` | The durable design record — one file per subsystem or precept, plus `AUDIT.md`, the punch list. Read this before proposing an architecture change. |
| `SPEC.md`, `PROGRESS.md` | Product vision (aspirational in places) and a running build snapshot. |

## Contributing

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

CI runs those on macOS (`cargo check`/`clippy`/`fmt --check`/`test`), plus
`cue vet schema/*.cue` and a no-push build of `sidecar/Dockerfile`. Note that
the UI build is a *prerequisite* of every Rust job, not a nicety:
`include_dir!("$CARGO_MANIFEST_DIR/ui/dist")` is a proc macro that panics at
compile time if `ui/dist` is missing, and it's gitignored. The tests that
need a real Docker daemon and a real root `fghjd` can't run in CI at all —
run those locally.

Before changing behaviour, check `concepts/` for a file that already explains
why the current behaviour is what it is — and when you settle something new,
fold the decision into the relevant concept file rather than leaving it only
in a commit message.
