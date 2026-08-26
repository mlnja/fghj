---
title: fghj wire
description: Register a workspace with the running fghjd daemon so it shows up in the UI.
---

```bash
fghj wire <entry> [--workspace <path>]
```

Wires a workspace into the currently running `fghjd`, so it shows up in
the web UI and can have runs started against it.

## Arguments

| Argument | Description |
|---|---|
| `entry` | Git URL of the entry repo. Cloned into the workspace if it isn't already checked out there. |
| `--workspace <path>` | Workspace root directory holding sibling repo checkouts. Defaults to the current directory. |

## Requirements

`fghjd` must already be running (`sudo fghjd`) — `wire` fails immediately
with a clear error if it can't find the daemon's control API.

## What it does

1. Captures your identity — uid, gid, `$HOME`, and your SSH agent socket —
   if `$HOME` is set. This is what lets the root-owned `fghjd` clone
   private repos over SSH on your behalf later, without ever holding your
   credentials itself; see
   [Persistence & workspace store](/concepts/persistence-and-workspace-store/)
   for the full mechanism.
2. Sends the entry repo and workspace path to `fghjd`'s control API, which
   registers the workspace (and clones the entry repo if needed).
3. Prints a link to open the workspace in the UI:
   `https://fghj.internal/?workspace=<id>`.

Run `fghj wire` again for the same workspace any time your SSH agent has
restarted — the captured identity is refreshed on every call, since the
agent socket it points at is only valid for the login session that was
live when it was captured.

`wire` only registers the workspace and resolves its graph — it doesn't
start any containers. Use the UI's "Pull all" and "Start default
environment" (or `fghj graph` to inspect what would be resolved) as the
next steps; see [Quickstart](/getting-started/quickstart/).
