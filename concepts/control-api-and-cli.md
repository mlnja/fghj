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
- **Runs**: `GET`/`POST /runs`, `POST /runs/{run_id}/stop`, and per-node
  `POST .../nodes/{node_id}/start`|`stop`|`delete` — wrappers over
  `runs::RunRegistry` (see [[run-lifecycle-and-registry]]); `post_runs` is
  the one handler that decides `start` vs. `ensure_running` based on
  whether the request specified a `run_id`. `POST .../debug-wait` is the one
  per-node route with a request body (`{"wait": bool}`); it flips a value in
  `desired` and reuses the ordinary `Starting` convergence rather than
  adding an action of its own (see [[debugging-in-containers]]).
- **Per-node observation**: `GET .../logs` (a snapshot),  `.../logs/stream`
  (SSE, live), `.../logs/generations` and `.../logs/history` (previous
  container incarnations of the same node, which is what makes a crash loop
  readable), and `.../events` (the node's own lifecycle/error record).
- **Exec**: `GET .../nodes/{node_id}/exec/ws`, the one WebSocket route —
  see below.
- **Daemon control and telemetry**: `POST /daemon/start`|`stop`,
  `GET /daemon/status`, `GET /daemon/logs`, `GET /daemon/net-status`. These
  are the only routes with no `?workspace=` — they hang off `DaemonControl`
  rather than `WorkspaceRegistry`, because they are about the process, not
  about anything wired into it.
- **Everything else**: falls back to `static_handler`, which serves the
  embedded Svelte build (`server::UI_DIST`, baked in at compile time via
  `include_dir!("$CARGO_MANIFEST_DIR/ui/dist")`) with an SPA-style
  fallback to `index.html` for any route the bundle doesn't literally
  contain — this is what makes client-side routes work on a hard refresh.

## `fghj exec`: one WebSocket, because nothing else is full duplex

Every other route is request/response, which is why `exec` is the only
WebSocket in the system. An interactive shell needs bytes moving both
directions at once, indefinitely, with no framing the HTTP request/response
cycle can express — so `GET .../exec/ws` upgrades, and after that the socket
carries a small protocol of its own: one `ExecStart` message (command, `tty`,
initial `cols`/`rows`), then raw stdin bytes in binary frames, with resize
notifications interleaved as the one other kind of text frame.

The CLI half is where the interesting decisions are. TTY allocation
auto-detects exactly the way `docker compose exec` does — on when *both*
local stdin and stdout are real terminals, off otherwise, with `-T` to force
it off — because a `fghj exec` inside a pipeline must behave like a plain
byte relay or every script using it breaks. When the TTY is on,
`RawModeGuard` puts local stdin into raw mode: no line buffering, no echo,
and crucially no signal generation, so Ctrl-C forwards to the *remote*
process as a byte instead of killing the local `fghj`. It restores the
original termios on `Drop`, which covers the error and early-return paths
that an explicit restore call at the end of the happy path would not — the
failure mode being a user left with a terminal that no longer echoes.

Resize forwarding is the small piece that is easy to skip and immediately
noticeable when missing: `SIGWINCH` on the CLI side sends the new
`cols`/`rows` up the socket, and the handler calls `docker::exec_resize`, so
a full-screen program inside the exec redraws correctly when the window
changes. The exec's real exit code is read back with
`docker::exec_exit_code` after the output stream ends and becomes `fghj`'s
own exit status, so `fghj exec ... && something` works.

## Telemetry: what the daemon knows about itself

`fghjd` has no logging crate and no log file. Its `println!`/`eprintln!` call
sites go through `daemon_log::info`/`warn`, which additionally append to a
process-global ring buffer capped at 2000 entries — deliberately in-memory
and reset on every restart, an operator's recent-activity view rather than a
durable audit log. `GET /daemon/logs` serves it, and the UI *polls* rather
than streaming: unlike a container's stdout this is low-volume,
operator-facing reconcile chatter, so a short poll is as responsive as SSE
and much less machinery.

The buffer's one subtlety is why the sequence counter lives inside the same
mutex as the deque. It used to be a separate `AtomicU64`, with the sequence
allocated *before* taking the lock — so two concurrent writers could take 5
and 6 and then append in the other order. A poller asks for "everything after
the last seq I saw", so a poller that had already seen 6 would filter out 5
forever: a silently lost log line, which is the one failure a log may not
have. `fghjd` genuinely has concurrent writers (converge tasks, DNS,
`raw_net`), so this was reachable in normal operation. Allocating the seq
under the append's own lock makes seq order and insertion order the same
thing by construction.

`GET /daemon/net-status` is the other half, and its design rule is worth
stating explicitly: it reads the three native-OS integration points back
from their *actual* live state — `/etc/hosts`, the `/etc/resolver` files
(via `dns::managed_resolver_zones`, which parses fghjd's own template to
recognize what it wrote), and `raw_net::current_routes` — rather than
reporting what fghjd last computed as desired. Reporting the desired state
would make the endpoint agree with fghjd by construction and therefore be
incapable of showing the one thing worth showing: that something outside
fghjd changed what fghjd installed. The reverse virtual-IP → raw-domain
mapping is reconstructed at the handler from the registry's current
endpoints, so `raw_net` doesn't have to carry a domain field through its
internal state purely for display. See [[two-zones-and-raw-ports]] for what
those routes are.

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

Implemented: `src/main.rs` (`validate`/`graph`/`wire`/`exec`/`daemon stop`),
`src/bin/fghjd.rs`, `src/daemon/` (`WorkspaceRegistry`, the full route
table, `spawn_reconciler`, `connect_docker`'s Docker-context fallback for
Docker Desktop/OrbStack/colima), `src/web/api/` (the axum router and its
handlers, including `exec.rs`'s WebSocket relay and `control.rs`'s
telemetry endpoints), `src/daemon_log.rs` (the ring buffer),
`src/web/ui.rs` (embedded UI serving). Known
mismatch: `SPEC.md`'s described CLI surface (`fghj setup`, `fghj up`,
`fghj branch`, `fghj branch set`) does not match what's actually
implemented — see `PROGRESS.md`'s "Known gaps" for the open question of
whether `SPEC.md` is aspirational or just stale.
