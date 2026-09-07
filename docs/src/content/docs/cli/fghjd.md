---
title: fghjd (superdaemon)
description: The root-owned background process — DNS, TLS proxy, local CA, Docker, and the control API.
---

```bash
sudo fghjd
```

The one root-owned superdaemon per machine. It doesn't take subcommands —
everything is driven through its HTTP control API by the `fghj` CLI or
the web UI.

## Requirements

- Must run as root — `fghjd` refuses to start otherwise, since it needs
  to bind ports 80/443 and write the system certificate trust store.
- Needs a reachable Docker daemon — `fghjd` connects and pings it as the
  very first startup step, so a bad Docker setup is reported immediately
  rather than causing `fghjd` to come up half-broken.
- Doesn't daemonize itself. For local development, run it directly in a
  terminal and leave it in the foreground. For a persistent install,
  supervise it with `systemd` or a `launchd` `LaunchDaemon`, which already
  handle backgrounding, restart-on-crash, and log capture — see
  [Control API](/concepts/control-api/).

## What it owns

- A hand-rolled authoritative DNS server for `*.fghj.internal` — see
  [Split DNS](/concepts/split-dns/).
- A local certificate authority and a TLS-terminating reverse proxy on
  ports 80/443 — see [Local CA & TLS proxy](/concepts/local-ca-and-tls-proxy/).
- The control API and the embedded web UI, served over
  `https://fghj.internal/`.
- Every wired workspace's run state and Docker containers — see
  [Run lifecycle & registry](/concepts/run-lifecycle-and-registry/) and
  [Persistence & workspace store](/concepts/persistence-and-workspace-store/).

## Startup order

Steps run in a specific, fail-fast order so problems surface immediately
instead of leaving `fghjd` half-up: Docker connectivity → control API
listener bind (TCP, internal) and control socket bind (Unix, for the CLI)
→ CA setup (generate or load, then install trust) → activation (DNS
server bind and OS resolver config, bind ports 80/443, sync
`/etc/hosts`) → start serving. See [Control API](/concepts/control-api/)
for why this exact order matters.

## Going idle without stopping it

`fghjd` is meant to run for the life of the machine — use
[`fghj daemon stop`](/cli/daemon/) to release ports 80/443, DNS, and
`/etc/hosts` without killing the process. A real termination signal
(SIGTERM/SIGINT — service stop/restart, system shutdown) runs the same
cleanup before the process actually exits.
