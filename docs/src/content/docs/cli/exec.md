---
title: fghj exec
description: Run a command inside a running node's container — a full-duplex docker compose exec equivalent.
---

```bash
fghj exec <node> [--workspace <path>] [--run <run>] [-T] -- <cmd>...
```

Runs a command inside an already-running node's container, proxied full
duplex over `fghjd`'s control socket — the fghj equivalent of `docker
compose exec`. A real interactive shell works: arrow keys, tab-completion,
`Ctrl-C` interrupts the remote command (not the local `fghj` process), and
resizing your terminal resizes the remote one too.

## Arguments

| Argument | Description |
|---|---|
| `node` | Node id to exec into — see `fghj graph` for ids. Its container must already be running. |
| `--workspace <path>` | Workspace root directory holding sibling repo checkouts. Defaults to [the wired workspace you're standing in](#the-workspace-is-derived-from-where-you-are), falling back to the current directory. |
| `--run <run>` | Which run to target. Defaults to `default`, the shared default environment. |
| `-T`, `--no-tty` | Disable pseudo-TTY allocation even if stdin/stdout are real terminals. |
| `<cmd>...` | The command and its arguments to run inside the container. Put `--` before it if it has flags of its own that could otherwise confuse `fghj`'s own argument parsing. |


## The workspace is derived from where you are

`--workspace` is optional because fghj works out which workspace you mean
from your current directory. It asks `fghjd` for the list of wired
workspaces and picks the one your directory is inside — at any depth, so
this works from the workspace root, from a repo inside it, or from a
subdirectory five levels down inside that repo:

```bash
cd ~/aikido/aikifactory/internal/config
fghj exec api -- bash
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

## Requirements

`fghjd` must be running and the workspace must already be
[wired](/cli/wire/) with the target node's container started (via the UI or
`fghj graph`/the run API) — `exec` fails immediately with a clear error if
either isn't true.

## TTY behavior

TTY allocation auto-detects the same way `docker compose exec` does: on when
both local stdin and stdout are real terminals, off otherwise (or always off
with `-T`). Without a TTY, `exec` still proxies stdin/stdout full duplex —
useful for piping input into a command or scripting a one-off command
without a pseudo-terminal's line-editing getting in the way:

```bash
echo "select 1;" | fghj exec -T db.storefront -- psql -U shop shop
```

## Examples

```bash
# a real interactive shell
fghj exec web.storefront -- bash

# a one-off command, output streamed back, exit code propagated
fghj exec db.storefront -- pg_isready
```
