---
title: fghj daemon
description: Toggle fghjd between active and idle without stopping the process itself.
---

```bash
fghj daemon start
fghj daemon stop
fghj daemon restart
fghj daemon status
```

`fghjd` itself is meant to run for the life of the machine — started at
boot and restarted on crash by whatever supervises it (`systemd`, a
`launchd` `LaunchDaemon`, or just a terminal you leave open during local
dev). None of these subcommands stop or start that process. Instead they
talk to its always-on control API to toggle whether it's *active*
(occupying ports 80/443, answering `*.fghj.internal` DNS, and maintaining
`/etc/hosts`) or *idle* (staying up and reachable, but out of the way).

## `fghj daemon stop`

Tells `fghjd` to go idle:

1. Stops and drops the DNS server and the HTTP/HTTPS reverse proxy,
   freeing ports 80/443 and the DNS socket.
2. Removes the (on macOS) `/etc/resolver/fghj.internal` resolver config.
3. Clears fghj's managed block from `/etc/hosts`, dropping every
   [additional host](/reference/fghj-yaml/#additional-hosts) alias.

`fghjd` keeps running and keeps serving its control API the whole time —
this is why no `sudo` is needed for it, unlike the old kill-based version
of this command.

Docker containers `fghjd` was fronting are **not** stopped — they keep
running under Docker's own supervision and are simply unreachable via
`*.fghj.internal` or any additional host until the next `start`.

If `fghjd` isn't running at all, this just prints `fghjd is not running`
and exits cleanly.

This is a durable instruction, not a one-shot action: `fghjd` persists an
`idle_requested` flag in its on-disk state (`/var/lib/fghjd/daemon-state.json`)
so that if it crashes or the machine reboots before you run `start` again,
it comes back up **idle** instead of silently reactivating behind your
back. Only an explicit `fghj daemon start` clears that flag.

## `fghj daemon start`

Reverses `stop`: rebinds DNS and the 80/443 proxy, reinstalls the OS
resolver config, and resyncs `/etc/hosts` from whatever's currently
running. Safe to call when already active — it's a no-op in that case.

Because `fghjd`'s reconciler already re-derives all of this state from
live Docker containers and on-disk workspace metadata on every tick (see
[Run lifecycle & registry](/concepts/run-lifecycle-and-registry/)),
`start` doesn't need `fghjd` to remember what it was doing before `stop`
— it just reconciles fresh, the same way it would after an ungraceful
crash. It also clears the `idle_requested` flag described above, so a
later crash/reboot restart comes back active again.

## `fghj daemon restart`

Equivalent to `stop` followed by `start`.

## `fghj daemon status`

Reports whether `fghjd` is reachable at all, and if so, whether it's
currently active or idle.

## What actually stops the process

`fghjd` only exits in response to a real termination signal (SIGTERM/
SIGINT) — e.g. `sudo brew services stop fghj`, `launchctl bootout`, a
service uninstall, or system shutdown. On that path it still runs the same
deactivation as `fghj daemon stop` first, so it never leaves stale ports,
resolver config, or `/etc/hosts` entries behind for whatever starts next.
