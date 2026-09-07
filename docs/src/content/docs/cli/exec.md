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
| `--workspace <path>` | Workspace root directory holding sibling repo checkouts. Defaults to the current directory. |
| `--run <run>` | Which run to target. Defaults to `default`, the shared default environment. |
| `-T`, `--no-tty` | Disable pseudo-TTY allocation even if stdin/stdout are real terminals. |
| `<cmd>...` | The command and its arguments to run inside the container. Put `--` before it if it has flags of its own that could otherwise confuse `fghj`'s own argument parsing. |

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
echo "select 1;" | fghj exec -T postgres -- psql -U postgres
```

## Examples

```bash
# a real interactive shell
fghj exec postgres -- bash

# a one-off command, output streamed back, exit code propagated
fghj exec postgres -- pg_isready
```
