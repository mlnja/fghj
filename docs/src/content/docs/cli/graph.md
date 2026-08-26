---
title: fghj graph
description: Resolve the full dependency graph for a workspace and print it as JSON.
---

```bash
fghj graph <entry> [--workspace <path>]
```

Resolves the complete dependency universe reachable from an entry repo —
every flow it declares, every service and backing dependency they pull
in — and prints the resolved graph as JSON to stdout.

## Arguments

| Argument | Description |
|---|---|
| `entry` | Git URL of the entry repo. Cloned into the workspace if it isn't already checked out there. |
| `--workspace <path>` | Workspace root directory holding sibling repo checkouts. Defaults to the current directory. |

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
