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
| `--workspace <path>` | Workspace root directory holding sibling repo checkouts. Defaults to [the wired workspace you're standing in](#the-workspace-is-derived-from-where-you-are), falling back to the current directory. |


## The workspace is derived from where you are

`--workspace` is optional because fghj works out which workspace you mean
from your current directory. It asks `fghjd` for the list of wired
workspaces and picks the one your directory is inside — at any depth, so
this works from the workspace root, from a repo inside it, or from a
subdirectory five levels down inside that repo:

```bash
cd ~/aikido/aikifactory/internal/config
fghj wire https://github.com/you/storefront.git
```

When it derives a workspace you didn't name, it says so on stderr:

```
using workspace /Users/you/aikido (derived from the current directory)
```

Two things it deliberately does not do. It does not guess from the
filesystem — no walking up looking for an `.fghj.yaml` — so the only
directories it will ever choose are ones you explicitly wired. And if
`fghjd` isn't running, or your directory isn't inside any wired workspace,
it falls back to the current directory exactly as it did before, rather
than failing.

There is never an ambiguity to resolve, because workspaces cannot overlap:
`fghj wire` [refuses a directory that nests with an already-wired
one](/cli/wire/#workspaces-cannot-nest), in either direction.


## Workspaces cannot nest

`fghj wire` refuses a workspace that overlaps one already wired —
**in either direction**:

```
$ fghj wire <url> --workspace ~/work/shop/api
Error: /Users/you/work/shop/api is inside the already-wired workspace
/Users/you/work/shop. Workspaces cannot nest — wire it separately outside
that directory, or stop the outer workspace first.

$ fghj wire <url> --workspace ~/work
Error: /Users/you/work contains the already-wired workspace
/Users/you/work/shop. Workspaces cannot nest — wire a directory that
doesn't enclose it, or stop the inner workspace first.
```

A workspace is the unit [`fghj graph`](/cli/graph/) scans: every sibling
directory under it is a candidate repo. Two overlapping workspaces would
therefore claim the same repos, derive duplicate node ids and identical
`*.fghj.internal` domains from them, and run two independent reconcilers
over one set of containers. Nothing could resolve that afterwards, so it is
rejected up front — the same reasoning as the rule that two workspace
folders can't [sanitize to the same domain
segment](/concepts/node-identity-and-domains/).

Re-wiring the *same* path is fine, and is how you add another entry repo to
an existing workspace — it returns the workspace you already have rather
than a second one.

The check runs before anything is created, so a rejected `wire` does not
leave a half-cloned entry repo behind.

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

`wire` only registers the workspace — it doesn't resolve its graph or
start any containers. Graph resolution happens separately, either via
`fghj graph` or when the UI loads the workspace. Use the UI's "Pull all"
and "Start default environment" (or `fghj graph` to inspect what would be
resolved) as the next steps; see
[Quickstart](/getting-started/quickstart/).
