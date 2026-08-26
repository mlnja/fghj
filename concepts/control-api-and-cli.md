# The control API and the `fghj`/`fghjd` process split

## Two binaries, two privilege levels

`fghjd` (`src/bin/fghjd.rs`) is the one root-owned superdaemon per machine —
explicitly modeled on `dockerd`: one instance, many workspaces, the same way
one `dockerd` manages many containers. It refuses to start unless
`geteuid() == 0`, and it doesn't daemonize itself — for local dev you run it
directly (`sudo fghjd`) and it stays in the foreground; in a real install
it's meant to be supervised by systemd or a launchd `LaunchDaemon`, which
already handle backgrounding, restart-on-crash, and log capture, so
`fghjd` doesn't need to reimplement any of that.

`fghj` (`src/main.rs`) is the unprivileged CLI a developer actually runs day
to day (`validate`, `graph`, `wire`, `daemon stop`). It never touches Docker,
ports 80/443, or the CA directly — everything that needs root goes through
`fghjd`'s HTTP control API instead. This split is also *why*
[[persistence-and-workspace-store]]'s `WorkspaceOwner` capture exists at
all: `fghj wire` runs as the real user and can read `$HOME`/`$SSH_AUTH_SOCK`
directly, so it's the natural place to capture that identity and hand it to
the root daemon that otherwise couldn't get it.

## Discovering the daemon without a fixed port

Neither the control API's port nor (historically) even its existence is
assumed by the CLI. `fghj::daemon::read_port()` reads
`/var/run/fghjd.port` (written by `run_control_api` right after binding —
see [[local-ca-and-tls-proxy]]'s "ephemeral-port + `/var/run` discovery"
section, which this follows the same pattern as); `probe_daemon` treats "can
I open a TCP connection to that port" as "is `fghjd` up" — cheaper and more
direct than also checking the pidfile. `fghj wire`'s success message points
at `https://fghj.internal/...` (the well-known, fixed TLS proxy address),
never a raw `127.0.0.1:<port>` URL — the control API's own ephemeral port is
an implementation detail the end user never needs to know.

## A hand-rolled HTTP client, deliberately

`http_post_json` builds a raw HTTP/1.1 POST over a plain `TcpStream` by
hand — no `reqwest`/`ureq` dependency — because the one thing it needs to do
(one POST, tiny JSON body, localhost, synchronous) doesn't justify pulling
in a full HTTP client crate. This mirrors the same "hand-roll it, the actual
surface is tiny" judgment call behind `src/dns.rs` (see [[split-dns]]).

## The axum control API

`daemon::build_router` is the one router every workspace's routes are served
from — there's no per-workspace listener or thread; concurrent requests for
different workspaces all flow through the same `axum::serve` loop, resolved
to the right `WorkspaceState` via `WorkspaceExtractor` (a custom
`FromRequestParts` that reads `?workspace=<id>` from the query string and
looks it up in the shared `WorkspaceRegistry`, or rejects with a 400 that
tells the caller to `POST /workspaces` first). This is what lets one `fghjd`
process serve the UI for several independently-wired workspaces
simultaneously, each with its own graph, runs, and download jobs, without
any of that being duplicated per-request.

Routes, grouped by what they front:

- **Workspace management**: `GET`/`POST /workspaces` (list / wire),
  `POST /workspaces/stop` (unwire and tear down every run in it).
- **Graph resolution**: `GET /universe.json` → `resolver::resolve_universe`,
  run in a blocking task since it shells out to `git` (see
  [[node-identity-and-domains]]).
- **Downloads**: `POST`/`GET /pull-all`(`/status`), `POST`/`GET /pull/{node_id}`
  (`/status`), `GET /pull-jobs` — thin wrappers over
  `downloads::DownloadRegistry` (see [[docker-and-downloads]]).
- **Runs**: `GET`/`POST /runs`, `POST /runs/{run_id}/stop`,
  `GET /runs/{run_id}/nodes/{node_id}/logs`(`/stream`) — wrappers over
  `runs::RunRegistry` (see [[run-lifecycle-and-registry]]); `post_runs` is
  the one handler that decides `start` vs. `ensure_running` based on
  whether the request specified a `run_id`.
- **Everything else**: falls back to `static_handler`, which serves the
  embedded Svelte build (`server::UI_DIST`, baked in at compile time via
  `include_dir!("$CARGO_MANIFEST_DIR/ui/dist")`) with an SPA-style
  fallback to `index.html` for any route the bundle doesn't literally
  contain — this is what makes client-side routes work on a hard refresh.

## Fail fast, in a specific order

`run_control_api` deliberately orders its startup steps so failures surface
as early and as clearly as possible, rather than leaving `fghjd` half-up:
connect to Docker and `ping()` it (a bad Docker setup is reported before
anything else even tries to start) → bind DNS and install the OS resolver
config → bind the control API's own listener (writing its port file) → set
up the CA (generate-or-load, then install trust) → bind ports 80/443 →
finally start serving. Binding 80/443 specifically happens before the
control API becomes reachable, so "something else already has that port"
is reported as a hard startup error instead of `fghjd` silently running
without any TLS proxy in front of it.

## Status

Implemented: `src/main.rs` (`validate`/`graph`/`wire`/`daemon stop`),
`src/bin/fghjd.rs`, `src/daemon.rs` (`WorkspaceRegistry`, the full route
table, `spawn_reconciler`, `connect_docker`'s Docker-context fallback for
Docker Desktop/OrbStack/colima), `src/server.rs` (embedded UI serving). Known
mismatch: `SPEC.md`'s described CLI surface (`fghj setup`, `fghj up`,
`fghj branch`, `fghj branch set`) does not match what's actually
implemented — see `PROGRESS.md`'s "Known gaps" for the open question of
whether `SPEC.md` is aspirational or just stale.
