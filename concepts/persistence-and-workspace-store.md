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
   the durable twin of `Orchestrator`'s in-memory state — reopened and
   reconciled against real Docker state on every daemon startup (see
   [[run-lifecycle-and-registry]]).

## Schema: reset, not migrated

Five tables: `owner` and `env` (one row each), `containers` (one row per
node: its `desired` and `observed` as JSON), `logs` and `events`.

There are no migrations. `PRAGMA user_version` carries a schema version, and
a database at any other version is dropped and recreated on open. That is
safe because nothing in it is irreplaceable: the environment is rebuilt from
Docker by the next start, and the owner is re-sent by the next `fghj wire`.
Keeping it that way is the price of the shortcut — anything that can't be
rebuilt doesn't belong in this database.

The previous design added columns with best-effort `ALTER TABLE` on every
open and read every column as optional, guessing what a missing one meant.
That cost ~100 lines and a test per column for compatibility with
pre-release databases.

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
and if not, asks the platform's `Session::session_ssh_agent` for the login
session's own agent, which is bound to the session rather than any one
terminal, to recover a working socket even when the original hint has gone
dead. On macOS that's the launchd agent (`com.apple.launchd.*/Listeners`
under `/private/var/run` since macOS 26, `/private/tmp` before); on Linux
the first live socket under `/run/user/<uid>` of GNOME's `gcr/ssh` (24.04
and later), GNOME Keyring's `keyring/ssh` (22.04), OpenSSH's own
`openssh_agent` user unit, and gpg-agent's `gnupg/S.gpg-agent.ssh`.
Anywhere else a dead hint is simply unusable.

A workspace with no recorded owner (one wired before owners were recorded,
or whose `.fghj` was recreated since) borrows whoever owns its folder
instead: uid/gid from the folder, home from the password database, and the
agent found by the same scan (`workspace_owner`). A folder owned by root
leaves nobody to borrow, so git runs as root. When the remote then refuses
the key, the pull log ends with a hint saying so and to run `fghj wire`
from your own user. A refusal while running as the owner gets a hint about
`ssh-add` instead. If the agent was the session's own, found by the scan, the hint
also says that keys kept in another agent (1Password and the like) need
`IdentityAgent` in `~/.ssh/config` or a `fghj wire` from the shell that has
their `SSH_AUTH_SOCK`.

The agent is borrowed for `docker build` too, not just `git clone` — but by a
different mechanism, because there is no subprocess to configure. bollard reads
`SSH_AUTH_SOCK` out of `fghjd`'s own environment, so the path
`live_ssh_auth_sock` returns has to be set process-globally around the build
rather than handed to a `Command`. See [[build-inputs]].

`harden_git_ssh` is applied to every git subprocess regardless — it forces
`BatchMode=yes` and auto-accepts unknown host keys via `GIT_SSH_COMMAND`,
so a clone either succeeds or fails fast and *visibly* (in the captured
stdout/stderr the UI streams — see [[docker-and-downloads]]) instead of
hanging forever on an interactive host-key prompt written straight to a
controlling tty that nothing capturing the subprocess's output could ever
see, let alone answer. It's applied even when also running as the real
owner, as a second line of defense.

## Status

Implemented: `src/persistence/` (`WorkspaceDb` in `persistence/sqlite/`, the workspace
index, `WorkspaceOwner` capture/apply, `live_ssh_auth_sock` recovery,
`harden_git_ssh`). The session-agent recovery knows macOS and Linux
(`src/platform/*/session.rs`, `runtime_dir.rs`); Windows isn't supported.
