# Persistence and the workspace store

## Two layers of "where things are recorded"

`fghjd` is a single long-lived root process that can own many workspaces at
once, and each workspace has its own durable state. That's two different
questions with two different answers:

1. **"Which workspaces exist at all, and where?"** — a single, root-owned
   index file, `/var/lib/fghjd/workspaces.json` (`store::default_index_path`),
   mapping workspace id → root path. Not `/var/run`: that's commonly a
   tmpfs wiped on reboot, which would defeat the entire point of
   remembering wired workspaces across a `fghjd` restart. `load_index` /
   `save_index` take the path as a parameter rather than hardcoding it, so
   tests can point it at a tempdir instead of the real root-owned location.
   A missing or corrupt index is treated as "no workspaces known yet", not
   a startup failure — `daemon::WorkspaceRegistry::load_from` independently
   re-verifies every entry against disk anyway (skipping, with a logged
   warning, any workspace whose path no longer exists).
2. **"What's actually running/owned in *this* workspace?"** — a per-workspace
   SQLite database at `<workspace>/.fghj/fghj.db` (`WorkspaceDb`),
   colocated with the workspace the same way `.git` is, so it travels with
   the checkout rather than living only in `fghjd`'s process memory. This is
   the durable twin of `RunRegistry`'s in-memory state — reopened and
   reconciled against real Docker state on every daemon startup (see
   [[run-lifecycle-and-registry]]).

## Schema and migrations

Three tables: `meta` (one row per workspace — id, entry URL, creation time,
plus the owner columns below), `runs` (run id, serialized overrides,
network name), `containers` (one row per container in a run, including a
`routes_json` column holding the serialized `PortRoute` list).

New columns (`owner_uid`/`owner_gid`/`owner_home`/`owner_ssh_auth_sock` on
`meta`, `routes_json` on `containers`) were added after the tables already
existed in deployed databases, and SQLite's `CREATE TABLE IF NOT EXISTS`
is a no-op against an existing table — it won't retroactively add a column.
`WorkspaceDb::open` runs a best-effort `ALTER TABLE ... ADD COLUMN` for each
such column on every open and just ignores the error when it's already
present, since SQLite has no `ADD COLUMN IF NOT EXISTS`.

## `WorkspaceOwner`: why `fghjd` needs to borrow a real user's identity

`fghjd` runs as root (it needs to bind ports 80/443 and write the system
trust store — see [[local-ca-and-tls-proxy]]), but root has no SSH
credentials of its own, and cloning a private repo requires them. The fix is
`store::WorkspaceOwner` — a snapshot of the real user's `uid`/`gid`/`home`/
`ssh_auth_sock`, captured by the *unprivileged* `fghj` CLI at `fghj wire`
time (when it has the correct identity naturally) and sent to `fghjd`,
which persists it in `WorkspaceDb` and later uses
`WorkspaceOwner::apply_to_command` to run `git clone` (and friends) as that
user instead of as root:

```rust
cmd.uid(self.uid).gid(self.gid).env("HOME", &self.home);
```

Root can read/use another user's ssh-agent socket despite not *being* that
user — root bypasses the file permission check on the socket — so this
works as long as `fghjd` knows where the socket actually is.

`set_owner` is refreshed on **every** `fghj wire` of a given workspace, not
just the first: the captured `ssh_auth_sock` is only valid for the login
session that was live when it was captured, and can go stale if the agent
restarts. `live_ssh_auth_sock` doesn't trust the stored hint blindly either
— it re-verifies the hint is still a live socket owned by the right uid,
and if not, falls back to scanning macOS's well-known launchd socket
location (`/private/tmp/com.apple.launchd.*/Listeners`, which macOS's
*login-session-scoped* system agent is bound to, not any one terminal) to
recover a working socket even when the original hint has gone dead. On
other platforms there's no equivalent well-known path, so a dead hint is
simply unusable there.

`harden_git_ssh` is applied to every git subprocess regardless — it forces
`BatchMode=yes` and auto-accepts unknown host keys via `GIT_SSH_COMMAND`,
so a clone either succeeds or fails fast and *visibly* (in the captured
stdout/stderr the UI streams — see [[docker-and-downloads]]) instead of
hanging forever on an interactive host-key prompt written straight to a
controlling tty that nothing capturing the subprocess's output could ever
see, let alone answer. It's applied even when also running as the real
owner, as a second line of defense.

## Status

Implemented: `src/store.rs` (`WorkspaceDb` with migrations, the workspace
index, `WorkspaceOwner` capture/apply, `live_ssh_auth_sock` recovery,
`harden_git_ssh`). The owner-capture/privilege-drop path is macOS-aware
(the launchd socket recovery); Linux/Windows would need different
ssh-agent discovery if this is ever ported.
