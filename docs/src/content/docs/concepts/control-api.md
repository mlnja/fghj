---
title: Control API
description: The axum control API fghjd exposes, the fghj/fghjd process split, and how the CLI discovers and talks to the daemon.
---

## Two binaries, two privilege levels

`fghjd` is the one root-owned superdaemon per machine — explicitly modeled
on `dockerd`: one instance, many workspaces, the same way one `dockerd`
manages many containers. It refuses to start unless it's running as root,
and it doesn't daemonize itself — for local dev you run it directly
(`sudo fghjd`) and it stays in the foreground; in a real install it's
meant to be supervised by `systemd` or a `launchd` `LaunchDaemon`, which
already handle backgrounding, restart-on-crash, and log capture.

`fghj` is the unprivileged CLI you actually run day to day (`validate`,
`graph`, `wire`, `daemon start`/`stop`/`restart`/`status`). It never
touches Docker, ports 80/443, or the CA directly — everything that needs
root goes through `fghjd`'s HTTP control API instead. This split is also
why `fghj wire` is the place that captures your SSH identity for the
daemon to borrow later — see
[Persistence & workspace store](/concepts/persistence-and-workspace-store/):
`fghj wire` runs as the real user and can read your home directory and SSH
agent socket directly, so it's the natural place to capture that identity
and hand it to the root daemon that otherwise couldn't get it.

## Discovering the daemon over a Unix socket, not a port

The CLI talks to `fghjd` over a fixed Unix domain socket
(`/var/run/fghjd.sock`), dockerd-style, rather than a TCP port — there's
nothing to pick, discover, or collide with, and reachability is a
filesystem permission (the socket is `chmod`ed `0666` right after bind,
since `fghjd` runs as root but `fghj` runs as the invoking user) instead
of "anything that can reach 127.0.0.1." "Can I connect to that socket
path" is the CLI's entire definition of "is `fghjd` up" (see
`probe_daemon` in `main.rs`). `fghj wire`'s success message points at the
well-known, fixed proxy address (`https://fghj.internal/...`), never the
socket path — that's purely a CLI-to-daemon implementation detail.

The control API is also reachable over an ordinary TCP loopback listener
on an OS-assigned port, but that one is never dialed by the CLI — it
exists only so the HTTPS reverse proxy can relay `https://fghj.internal`
traffic to it internally (see [Local CA & TLS proxy](/concepts/local-ca-and-tls-proxy/)).
Both listeners serve the exact same `axum::Router`.

## A hand-rolled HTTP client, deliberately

The CLI talks to `fghjd` with a raw HTTP/1.1 request built by hand over
the Unix socket — no HTTP client library — because the one thing it needs
to do (one request, a tiny JSON body, localhost, synchronous) doesn't
justify pulling in a full HTTP client dependency. This mirrors the same
"hand-roll it, the actual surface is tiny" judgment call behind fghj's own
DNS server — see [Split DNS](/concepts/split-dns/).

## The axum control API

One router serves every workspace's routes — there's no per-workspace
listener or thread; concurrent requests for different workspaces all flow
through the same server loop, resolved to the right workspace via a
`?workspace=<id>` query parameter (or rejected with a clear error telling
the caller to wire a workspace first). This is what lets one `fghjd`
process serve the UI for several independently wired workspaces
simultaneously, each with its own graph, runs, and download jobs, without
any of that being duplicated per request.

Routes, grouped by what they front:

- **Workspace management** — list/wire a workspace, unwire and tear down
  every run in it.
- **Graph resolution** — resolve the full dependency graph and serve it as
  JSON; see [Node identity & domains](/concepts/node-identity-and-domains/).
- **Downloads** — start/poll single-node, whole-graph, and flow-scoped
  pulls; see [Docker & downloads](/concepts/docker-and-downloads/).
- **Runs** — list/start/stop runs, stream a container's logs; see
  [Run lifecycle & registry](/concepts/run-lifecycle-and-registry/).
- **Everything else** falls back to serving the embedded Svelte build,
  with a single-page-app-style fallback to `index.html` for any route the
  bundle doesn't literally contain — this is what makes client-side
  routes work on a hard refresh.

## Fail fast, in a specific order

Startup deliberately orders its steps so failures surface as early and as
clearly as possible, rather than leaving `fghjd` half-up: connect to
Docker and ping it (a bad Docker setup is reported before anything else
even tries to start) → bind the control API's TCP listener and the CLI's
Unix socket → set up the CA (generate-or-load, then install trust) →
activate (bind DNS, install the OS resolver config, bind ports 80/443,
sync `/etc/hosts`) → finally start serving. Activation specifically
happens before the control API becomes reachable, so "something else
already has port 80/443" is reported as a hard startup error instead of
`fghjd` silently running without any TLS proxy in front of it.

## Active vs. idle

Everything activation does (DNS, 80/443, `/etc/hosts`) can be torn down
and rebuilt independently of the process itself — `DaemonControl` in
`daemon.rs` owns that toggle. `fghjd` starts active by default and stays
that way until `fghj daemon stop` deactivates it or a termination signal
(SIGTERM/SIGINT) shuts the whole process down after deactivating first.
Docker containers are untouched by either transition — see
[`fghj daemon`](/cli/daemon/) for the full behavior and why "stop, then
start" is safe to lean on (the reconciler re-derives state from scratch
either way).

Active/idle is in-memory state (`DaemonControl.active`), so it wouldn't
survive `fghjd` restarting on its own — a crash or a reboot would
otherwise silently reactivate a daemon the operator had explicitly told
to stay idle. `is_idle_requested`/`set_idle_requested` close that gap
against `store::DaemonState`, a small durable JSON file
(`/var/lib/fghjd/daemon-state.json`, alongside the CA and the workspace index
rather than under `/var/run` so it survives a reboot too): `post_daemon_stop`
sets its `idle_requested` field, `post_daemon_start` clears it, and startup
checks it before deciding whether to activate at all. `DaemonState` follows
the same plain-JSON, `#[serde(default)]`-fields pattern as the workspace
index (`store::load_index`/`save_index`) rather than SQLite — there's only
one writer and no relational structure, so it's expected to simply grow more
fields over time as `fghjd` accumulates other state worth surviving a
restart. The termination-signal path deliberately does *not* touch
`idle_requested` — it's mechanical cleanup for a process that's about to
die, not a change in operator intent, so whichever state was requested
before the signal is what comes back after the restart.
