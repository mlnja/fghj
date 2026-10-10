---
title: fghj uninstall
description: Removes the trusted root CA, /var/lib/fghjd, and every system change fghjd made.
---

```
sudo fghj uninstall [-y] [--keep-ca] [--keep-state]
```

Removes everything fghj installed on this machine except the binaries and
your Docker resources.

`brew uninstall` runs unprivileged, so two of fghj's artifacts survive it —
and neither is easy to find later:

- a **trusted root certificate** in `/Library/Keychains/System.keychain`
- `/var/lib/fghjd`, which holds that CA's **private key**, every wired
  workspace, and all run state

Leaving those behind means a trusted root whose key is still sitting on
disk. This command is the one that clears them.

## What it does, in order

1. Stops `fghjd` if it is running, so the daemon unwinds its own system
   configuration.
2. Clears the managed `/etc/hosts` block, `/etc/resolver/fghj*`, the `lo0`
   aliases and the pf anchor directly. This is belt and braces: a daemon
   that was killed with `SIGKILL` never ran its own teardown, so these can
   still be installed with nothing alive to remove them. Each step is a
   no-op when there is nothing to undo.
3. Deletes every `fghj local CA` certificate from the System keychain.
4. Removes `/var/lib/fghjd`.

The order matters. A live `fghjd` would re-mint and re-trust a CA on its
next reconcile, undoing step 3 — so the daemon goes down first.

Step 3 loops rather than deleting once. `security delete-certificate`
removes a single match per call, and a machine that has been through
several installs can hold several: deleting `ca/` makes the next start mint
a *new* CA and trust that one too.

## Options

| Flag | Effect |
|---|---|
| `-y`, `--yes` | Skip the confirmation prompt |
| `--keep-ca` | Leave the root CA trusted — avoids a fresh keychain authorization prompt when reinstalling |
| `--keep-state` | Leave `/var/lib/fghjd` in place — keeps every wired workspace and the CA key |

Without `-y` it prints exactly what it will delete and waits. Anything but
`y` or `yes` — including a bare Enter, or a closed stdin under a pipe or in
CI — aborts without removing anything.

## It needs root, and won't escalate for you

All three of its jobs require it: the System keychain, `/etc`, and
`/var/lib/fghjd`. Run unprivileged, it refuses and tells you what to run:

```
$ fghj uninstall
Error: fghj uninstall needs root — it removes a certificate from the System
keychain, /private/var/lib/fghjd, and fghjd's system configuration.

Run: sudo fghj uninstall
```

It deliberately does not re-exec itself under `sudo`. A command that
silently escalates in order to delete a trusted root certificate is the
wrong shape, however convenient.

## What it does not remove

**The binaries.** Use `brew uninstall fghj`.

**Docker containers, networks, volumes and images.** Not an oversight: a
named volume is *designed* to outlive its container and may hold a
database you still want, and fghj identifies its own resources by an
`fghj-` name prefix — a filter, not proof of ownership. Deleting data on a
name match is not a call an uninstall command gets to make. It points you
at them instead:

```bash
docker ps -a --filter "name=^fghj-" --format '{{.Names}}'
```

**Repositories fghj cloned** into your workspace. They are ordinary git
clones that may hold unpushed work.

See [What fghj touches, and how to remove it](/guides/uninstalling/) for the
full inventory and for doing any of this by hand.
