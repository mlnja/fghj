---
title: fghj daemon stop
description: Stop fghjd and every workspace it was serving.
---

```bash
fghj daemon stop
```

Stops the running `fghjd` process and cleans up the files it wrote while
it was up.

## What it does

If a live `fghjd` is found (via its pidfile), this:

1. Runs `sudo kill <pid>` to stop it.
2. Removes `fghjd`'s pidfile, port file, and (on macOS) the
   `/etc/resolver/fghj.internal` resolver config it installed —
   cleanup is safe to run unconditionally even on platforms where some of
   those files were never written.

Requires `sudo`, since `fghjd` runs as root.

If `fghjd` isn't running, this just prints `fghjd is not running` and
exits cleanly — it's safe to call even when you're not sure whether the
daemon is up.

## What it doesn't do

Stopping the daemon doesn't stop or remove any Docker containers it
started — those keep running until you stop them yourself (`docker
stop`/`docker rm`, or via `docker compose`-style tooling, since `fghj`
labels its containers to be visible there too — see
[Docker & downloads](/concepts/docker-and-downloads/)). The next time
`fghjd` starts back up, its startup reconciler will re-discover any
containers that are still alive and restore their run state; see
[Run lifecycle & registry](/concepts/run-lifecycle-and-registry/).
