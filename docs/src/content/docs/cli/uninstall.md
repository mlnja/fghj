---
title: fghj uninstall
description: Removes the trusted root CA, /var/lib/fghjd, and every system change fghjd made.
---

```
sudo fghj uninstall [-y] [--keep-ca] [--keep-state | --keep-workspaces]
```

Removes everything fghj installed on this machine except the binaries and
your Docker resources.

Removing the binaries alone leaves fghj's two biggest artifacts behind, and
neither is easy to find later:

- a **trusted root certificate**: in `/Library/Keychains/System.keychain` on
  macOS; on Linux in the system store and in your browsers' databases
- `/var/lib/fghjd`, which holds that CA's **private key**, every wired
  workspace, and all run state

Leaving those behind means a trusted root whose key is still sitting on
disk. This command is the one that clears them. On macOS run it before
`brew uninstall`, which runs unprivileged and can't; on Ubuntu `apt remove`
runs it for you.

## What it does, in order

1. Stops `fghjd` if it is running, so the daemon unwinds its own system
   configuration.
2. Clears the managed `/etc/hosts` block, the OS DNS routing
   (`/etc/resolver/fghj*` and the `lo0` aliases on macOS, the `fghj0` link
   on Linux) directly. This is belt and braces: a daemon that was
   killed with `SIGKILL` never ran its own teardown, so these can still be
   installed with nothing alive to remove them. Each step is a
   no-op when there is nothing to undo.
3. Deletes every `fghj local CA` certificate from the system trust store
   (the System keychain on macOS; `/usr/local/share/ca-certificates` on
   Linux) and, on Linux, from the browser databases of every wired
   workspace's owner and of the user who ran `sudo`.
4. Has fghjd remove fghj's sidecar containers, which hold the signing CA's
   key. Your own containers, networks and volumes stay. If fghjd isn't
   running, it can't, and the warning gives the `docker rm` command that
   does.
5. Removes `/var/lib/fghjd`, and fghj's own database in each wired
   workspace (`.fghj/fghj.db`). Nothing else in a workspace is touched.

Once it's done, no CA of fghj's exists anywhere: not trusted, not on disk,
not in a running container.

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
| `--keep-ca` | Leave the root CA trusted — on macOS, avoids a fresh keychain authorization prompt when reinstalling |
| `--keep-state` | Leave `/var/lib/fghjd` in place — keeps every wired workspace and the CA keys, and leaves the sidecars running |
| `--keep-workspaces` | Keep only the workspace index and daemon settings; every CA file and key still goes, and so do the sidecars. A reinstall mints a new CA and still knows your workspaces. Removing the Ubuntu package runs this |

Without `-y` it prints exactly what it will delete and waits. Anything but
`y` or `yes` — including a bare Enter, or a closed stdin under a pipe or in
CI — aborts without removing anything.

## It needs root, and won't escalate for you

All of its jobs require it: the system trust store, `/etc`, and
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

**The binaries.** Use `brew uninstall fghj` on macOS. On Ubuntu,
`sudo apt remove fghj` removes them and runs this command first.

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
