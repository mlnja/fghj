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
  to bind ports 80/443, alias its loopback address on macOS, route DNS
  through the OS, and write the system certificate trust store.
  (`fghjd --version` is the one thing it answers without root.)
- Uses the Docker your first [`fghj wire`](/cli/wire/) names, saved across
  restarts; before that, `/var/run/docker.sock` if it's live. Docker trouble
  never stops it starting: [`fghj doctor`](/cli/doctor/) reports it.
- Doesn't daemonize itself. For local development, run it directly in a
  terminal and leave it in the foreground. For a persistent install,
  supervise it with `systemd` or a `launchd` `LaunchDaemon`, which already
  handle backgrounding, restart-on-crash, and log capture — see
  [Control API](/concepts/control-api/).

## What it owns

- A hand-rolled authoritative DNS server for `*.fghj.internal` — see
  [Split DNS](/concepts/split-dns/).
- A local certificate authority and a TLS-terminating reverse proxy on
  ports 80/443 on `127.222.0.1` — see [Local CA & TLS proxy](/concepts/local-ca-and-tls-proxy/).
- The control API and the embedded web UI, served over
  `https://fghj.internal/`.
- Every wired workspace's run state and Docker containers — see
  [Run lifecycle & registry](/concepts/run-lifecycle-and-registry/) and
  [Persistence & workspace store](/concepts/persistence-and-workspace-store/).

## Startup order

Steps run in a specific, fail-fast order so problems surface immediately
instead of leaving `fghjd` half-up: Docker connectivity → control API
listener bind (TCP, internal) and control socket bind (Unix, for the CLI)
→ CA setup (generate or load, then trust it if it isn't already) → activation (DNS
server bind and OS resolver config, alias `127.222.0.1` and bind ports 80/443 on it, sync
`/etc/hosts`) → start serving. See [Control API](/concepts/control-api/)
for why this exact order matters.

On macOS, trusting the CA needs a password dialog, so it only works when
`fghjd` runs in a terminal. Under `brew services` there's no login session
to show one in: `fghjd` logs that and carries on, and HTTPS shows a
certificate warning until your first [`fghj wire`](/cli/wire/), which trusts
it from your terminal, as you. On Linux nothing needs asking: `fghjd` puts
the CA in the system store on every start, and in each workspace owner's
browser databases, as that owner. If trust is lost later,
`fghj doctor --fix` restores it.

## Going idle without stopping it

`fghjd` is meant to run for the life of the machine — use
[`fghj daemon stop`](/cli/daemon/) to release ports 80/443, its loopback alias, DNS, and
`/etc/hosts` without killing the process. A real termination signal
(SIGTERM/SIGINT — service stop/restart, system shutdown) runs the same
cleanup before the process actually exits.
