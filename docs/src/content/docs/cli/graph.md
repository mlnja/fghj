---
title: fghj graph
description: Resolve the full dependency graph for a workspace and print it as JSON.
---

```bash
fghj graph <entry> [--workspace <path>]
```

Resolves the complete dependency universe reachable from an entry repo —
every flow it declares, every service, database and task they pull
in — and prints the resolved graph as JSON to stdout.

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
fghj graph https://github.com/you/storefront.git
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

## What's in the output

The same resolved graph the web UI's Repos/Actual tabs render — nodes
(services and backing dependencies, each with its derived id, label, and
default-run domain) and the edges between them, plus which flows each
node belongs to. See
[Node identity & domains](/concepts/node-identity-and-domains/) for how
those ids and domains are derived.

Unlike `fghj wire`, `graph` doesn't require `fghjd` to be running — it
resolves the graph locally and prints it, without registering anything
with the daemon or touching Docker. It's useful for inspecting what a
workspace would resolve to, or piping into another tool, without
affecting the running daemon's state.

Because resolution is lazy (see
[Flat workspace model](/concepts/flat-workspace-model/)), a dependency
that isn't cloned into the workspace yet still appears in the output as a
stub node — check each node's `downloaded` field.
