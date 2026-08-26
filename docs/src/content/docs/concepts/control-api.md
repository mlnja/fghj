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
`graph`, `wire`, `daemon stop`). It never touches Docker, ports 80/443, or
the CA directly — everything that needs root goes through `fghjd`'s HTTP
control API instead. This split is also why `fghj wire` is the place that
captures your SSH identity for the daemon to borrow later — see
[Persistence & workspace store](/concepts/persistence-and-workspace-store/):
`fghj wire` runs as the real user and can read your home directory and SSH
agent socket directly, so it's the natural place to capture that identity
and hand it to the root daemon that otherwise couldn't get it.

## Discovering the daemon without a fixed port

Neither the control API's port nor its existence is assumed by the CLI —
the CLI reads a port file `fghjd` writes right after binding, and treats
"can I open a TCP connection to that port" as "is `fghjd` up." `fghj
wire`'s success message points at the well-known, fixed proxy address
(`https://fghj.internal/...`), never a raw `127.0.0.1:<port>` URL — the
control API's own ephemeral port is an implementation detail you never
need to know.

## A hand-rolled HTTP client, deliberately

The CLI talks to `fghjd` with a raw HTTP/1.1 POST built by hand over a
plain TCP connection — no HTTP client library — because the one thing it
needs to do (one POST, a tiny JSON body, localhost, synchronous) doesn't
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
even tries to start) → bind DNS and install the OS resolver config → bind
the control API's own listener → set up the CA (generate-or-load, then
install trust) → bind ports 80/443 → finally start serving. Binding
80/443 specifically happens before the control API becomes reachable, so
"something else already has that port" is reported as a hard startup
error instead of `fghjd` silently running without any TLS proxy in front
of it.
