---
title: Persistence & workspace store
description: The two layers of durable state fghjd keeps, and how a root daemon borrows a real user's identity to clone private repos.
---

## Two layers of "where things are recorded"

`fghjd` is a single long-lived root process that can own many workspaces
at once, and each workspace has its own durable state. That's two
different questions with two different answers:

1. **"Which workspaces exist at all, and where?"** — a single, root-owned
   index file mapping workspace id to root path. Not stored in a
   tmpfs-backed location, since that would defeat the entire point of
   remembering wired workspaces across an `fghjd` restart. A missing or
   corrupt index is treated as "no workspaces known yet," not a startup
   failure — every entry is independently re-verified against disk anyway,
   skipping (with a logged warning) any workspace whose path no longer
   exists.
2. **"What's actually running/owned in *this* workspace?"** — a
   per-workspace SQLite database colocated with the workspace itself, the
   same way `.git` is, so it travels with the checkout rather than living
   only in `fghjd`'s process memory. This is the durable twin of the
   in-memory run registry — reopened and reconciled against real Docker
   state on every daemon startup, see
   [Run lifecycle & registry](/concepts/run-lifecycle-and-registry/).

## Schema and migrations

Three tables: workspace metadata (id, entry URL, creation time, plus the
owner columns below), runs (run id, serialized overrides, network name),
and containers (one row per container in a run, including its serialized
routes). New columns get added to existing tables over time; since SQLite
has no `ADD COLUMN IF NOT EXISTS`, opening the database runs a best-effort
`ALTER TABLE ... ADD COLUMN` for each such column and just ignores the
error when it's already present.

## Why fghjd needs to borrow a real user's identity

`fghjd` runs as root — it needs to bind ports 80/443 and write the system
trust store (see [Local CA & TLS proxy](/concepts/local-ca-and-tls-proxy/))
— but root has no SSH credentials of its own, and cloning a private repo
needs them. The fix is a snapshot of the real user's uid/gid/home/SSH
agent socket, captured by the *unprivileged* `fghj` CLI at `fghj wire`
time — when it naturally has the correct identity — and handed to
`fghjd`, which persists it and later uses it to run `git clone` (and
friends) as that user instead of as root. Root can read another user's
SSH agent socket despite not being that user, since root bypasses the
file-permission check on the socket — this works as long as `fghjd` knows
where the socket actually is.

This identity is refreshed on **every** `fghj wire` of a given workspace,
not just the first: the captured SSH agent socket is only valid for the
login session that was live when it was captured, and can go stale if the
agent restarts. On macOS, a stale hint is automatically recovered by
falling back to the OS's well-known agent socket location, scoped to the
current login session rather than any one terminal.

Every git subprocess `fghjd` runs also forces batch mode and
auto-accepts unknown host keys, so a clone either succeeds or fails fast
and *visibly* — in the captured output the UI streams, see
[Docker & downloads](/concepts/docker-and-downloads/) — instead of hanging
forever on an interactive host-key prompt that nothing capturing the
subprocess's output could ever see, let alone answer.

## Limitations

The owner-capture/privilege-drop path is macOS-aware (the fallback agent
socket recovery); Linux/Windows would need a different SSH agent
discovery mechanism if this is ever ported.
