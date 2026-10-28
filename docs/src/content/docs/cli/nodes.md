---
title: fghj start, stop, rm, debug-wait
description: Act on one node from the terminal; each waits until fghj has finished.
---

```bash
fghj start <node>           [--workspace <path>] [--json]
fghj stop <node>            [--workspace <path>] [--json]
fghj rm <node>              [--workspace <path>] [--json]
fghj debug-wait <node> on|off [--workspace <path>] [--json]
```

These are the UI's per-node buttons. Each prints fghjd's log of the action
as it goes, waits until it has settled, then prints
[`fghj status`](/cli/status/). Each exits non-zero if the action failed.

| Command | What it does |
|---|---|
| `start` | Starts the node and whatever it needs, whether or not it's in the current flow. |
| `stop` | Stops the node, and first the nodes running on top of it. |
| `rm` | Removes the node's container and fghj's record of it. Its volumes stay. |
| `debug-wait` | Turns the [halt switch](/guides/debugging/#the-one-variable-fghj-sets) on or off. Either way the node is recreated to apply it. |

`<node>` is the node's id (`api.files-api`), or just the part before the
first `.` (`api`) when no other node shares it. A name that matches
nothing, or more than one node, fails and lists the candidates. `start`
accepts any node in the workspace's graph. The others need a node that
already has a container record.

`Ctrl-C` stops the watching, not the action.
