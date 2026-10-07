---
title: What fghj touches, and how to remove it
description: Every file, system setting, Docker resource and loopback alias fghj creates, and how to remove all of it.
---

fghj is a root daemon that edits system configuration, so it is worth being
able to see exactly what it has done to your machine and undo all of it.
This page is the complete list.

The short version, if you just want it gone:

```bash
sudo fghj uninstall
brew uninstall fghj
```

`fghj uninstall` does everything `brew uninstall` can't: it stops the
daemon, unwinds its system configuration, deletes the root CA from your
System keychain, and removes `/var/lib/fghjd`. It prints what it is about
to remove and asks before doing any of it (`-y` skips the prompt).

It needs root, and it will not silently re-run itself under `sudo` — a
command that escalates on its own to delete a trusted root certificate is
the wrong shape, so it refuses and tells you what to run.

Two flags exist for the reinstalling-in-place case, where a full purge is
more than you want:

| Flag | Keeps |
|---|---|
| `--keep-ca` | The root CA trusted, so a reinstall doesn't re-prompt for keychain authorization |
| `--keep-state` | `/var/lib/fghjd` — every wired workspace and both CAs' private keys |

Docker resources are left alone either way. See
[Docker resources](#docker-resources) below.

If you'd rather do it by hand, or `fghj` is already gone from your PATH,
every step is spelled out in the sections that follow.

## What is removed automatically

`fghj daemon stop` (and stopping the process) reverses every *system*
change. These are unwound on a clean shutdown, and re-applied on the next
start — you do not need to clean them up by hand unless `fghjd` was killed
uncleanly.

| What | Where |
|---|---|
| Managed hosts block | `/etc/hosts`, between `# fghj-managed-begin` and `# fghj-managed-end` |
| Split-DNS resolver file | `/etc/resolver/fghj.internal` |
| Proxy loopback alias | `127.222.0.1` on `lo0` |
| Raw-zone loopback aliases | `10.222.0.0/16` addresses on `lo0` |
| pf redirect rules | the `fghjd` pf anchor |
| Control socket | `/var/run/fghjd.sock` |

A clean stop also reverts the one-time edit fghj makes to `/etc/pf.conf` —
an `rdr-anchor "fghjd"` hook line wrapped in
`# --- fghjd raw-net anchor hook ... ---` markers — and disables pf again if
fghjd was the process that enabled it.

If `fghjd` is killed with `SIGKILL` or the machine loses power, those two
markers and the hook line can be left behind. That is harmless: an anchor
hook with nothing loaded into it is a no-op. Delete the three marked lines
if you want the file pristine.

The next start self-heals all of the above. To verify by hand:

```bash
grep -n "fghj" /etc/hosts /etc/pf.conf
ls /etc/resolver/
ifconfig lo0 | grep -E "127\.222|10\.222"
sudo pfctl -a fghjd -s nat
```

## Durable state on disk

Everything `fghjd` persists lives under one root — `/var/lib/fghjd`
(really `/private/var/lib/fghjd` on macOS, since `/var` is a symlink). There
is no hidden per-user cache or dotfile directory: fghj reads `$HOME` only to
run `git clone` as you rather than as root, and never writes a config or
cache of its own there. The one thing that does land in your own
directories is the repositories it clones — see
[Repositories fghj cloned](#repositories-fghj-cloned).

| Path | What it is |
|---|---|
| `ca/ca-cert.pem`, `ca/ca-key.pem` | The local root CA. Deleting these means a new CA on next start, which must be re-trusted. |
| `ca/signing-cert.pem`, `ca/signing-key.pem` | The name-constrained signing CA that issues every leaf cert. Deleting these costs nothing — a new one is minted under the same root on next start, with no re-trusting. |
| `ca/signing-generation` | Which constraint set the signing CA above was built with. Deleting it just regenerates the signing CA. |
| `ca/cert.pem`, `ca/bundle.pem` | World-readable trust files, for containers that need to trust fghj's CA. |
| `certs/` | The same two trust files, in a directory with no key material in it, bind-mounted read-only into every container at `/etc/fghj/certs`. |
| `workspaces.json` | The index of wired workspaces. |
| `daemon-state.json` | Daemon state across restarts. |
| `runs/<network>/` | Per-run route tables, read by each run's sidecar. |
| `sidecar-ca/` | A world-readable copy of the **signing** CA (certificate and key), mounted into sidecar containers. Never the root's key. |
| `sidecar-build/` | Scratch space for building the sidecar image from source. |

```bash
sudo rm -rf /var/lib/fghjd
```

Removing this is safe while the daemon is stopped. It forgets every wired
workspace and discards the CA.

## The root CA in your keychain

This is the one change that outlives `brew uninstall`, because it is not a
file fghj owns — it is an entry in the **System keychain**, installed with
`security add-trusted-cert` so your browser trusts `*.fghj.internal`
certificates.

```bash
# Confirm it is there
security find-certificate -c "fghj local CA" /Library/Keychains/System.keychain

# Remove it
sudo security delete-certificate -c "fghj local CA" \
  /Library/Keychains/System.keychain
```

Leaving a stale local CA trusted is not catastrophic — the root's private
key is in `/var/lib/fghjd/ca/`, root-readable only, and never leaves it —
but there is no reason to keep a trusted root you no longer use. Remove it.

Only the root is ever in your keychain, under the name above. The signing CA
is called `fghj signing CA` and is trusted transitively through the root, so
there is nothing separate to delete for it.

## Docker resources

These are **not** removed by stopping or uninstalling fghj. Containers stop
being reachable, but they keep running under Docker's own supervision.

Everything fghj creates is prefixed `fghj-`:

| Resource | Name |
|---|---|
| Containers | `fghj-<workspace>-<run-id>-<node>` |
| Sidecars | `fghj-<workspace>-<run-id>-sidecar` |
| Networks | `fghj-<workspace>-<run-id>` |
| Volumes | `fghj-vol-<derived>` |
| Sidecar image | `ghcr.io/mlnja/fghj-sidecar` |

:::caution[Check before you delete]
Run the `ls` form of each command first. The `fghj-` prefix is fghj's, but
these commands are a plain name filter — if you have unrelated resources
whose names begin with `fghj-`, they will match too.
:::

```bash
# Look first
docker ps -a  --filter "name=^fghj-" --format '{{.Names}}'
docker network ls --filter "name=^fghj-" --format '{{.Name}}'
docker volume ls  --filter "name=^fghj-" --format '{{.Name}}'

# Then remove
docker rm -f      $(docker ps -aq --filter "name=^fghj-")
docker network rm $(docker network ls -q --filter "name=^fghj-")
docker volume rm  $(docker volume ls -q  --filter "name=^fghj-")
docker rmi        $(docker images -q "ghcr.io/mlnja/fghj-sidecar")
```

Volumes are the ones worth pausing over: a `scope: stable` volume is
deliberately designed to survive across runs, so it may hold database
contents you still want. `docker volume ls --filter "name=^fghj-"` before
deleting.

Images your services were *built from* are not listed here — those are
ordinary Docker images built from your own `Dockerfile`s and are not named
by fghj.

## Repositories fghj cloned

Worth knowing, because it surprises people: when a `.fghj.yaml` declares a
`kind: service` dependency on another repo, fghj clones it **into your
workspace directory**, alongside your own repos — not into a hidden cache.

```
my-workspace/
  storefront/     <- yours, the one you cloned
  catalog/        <- cloned by fghj, from the dependency declaration
```

They are ordinary git clones with a normal `origin`, so nothing special is
needed to remove them — `rm -rf` the ones you did not create. Check for
unpushed work first; fghj never deletes them for you precisely because it
cannot know whether you have been editing them.

## Homebrew's own files

```bash
brew uninstall fghj
```

This removes the `fghj`, `fghjd` and `fghj-sidecar` binaries and the
LaunchDaemon plist Homebrew installed at
`/Library/LaunchDaemons/homebrew.mxcl.fghj.plist`. Stop the service first
(`sudo brew services stop fghj`) so the system changes above are unwound
while the daemon is still alive to unwind them.

To also drop the tap:

```bash
brew untap mlnja/tap
```

## Verifying nothing is left

```bash
# No process, no socket
pgrep -l fghjd; ls /var/run/fghjd.sock 2>/dev/null

# No system config
grep -c fghj /etc/hosts; ls /etc/resolver/fghj.internal 2>/dev/null
ifconfig lo0 | grep -E "127\.222|10\.222"
security find-certificate -c "fghj local CA" \
  /Library/Keychains/System.keychain 2>/dev/null

# No state, no Docker resources
ls /var/lib/fghjd 2>/dev/null
docker ps -a --filter "name=^fghj-" --format '{{.Names}}'
```

Every one of those should print nothing (or "not found"). If `pgrep` still
finds `fghjd`, something is supervising it — check
`sudo launchctl list | grep fghj`.
