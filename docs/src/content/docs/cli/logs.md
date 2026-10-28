---
title: fghj logs, fghj events
description: A node's container output, and fghj's own step-by-step account of starting or stopping it.
---

## fghj logs

```bash
fghj logs <node> [-f] [--tail <n>] [--workspace <path>] [--json]
```

Prints the last `--tail` lines of the node's container output (200 by
default). With `-f` it then keeps printing new lines until you press
`Ctrl-C`, like `docker logs -f`.

## fghj events

```bash
fghj events <node> [--stop] [--workspace <path>] [--json]
```

fghj's own account of the last time it started the node, step by step: the
same timeline as the node's drawer in the UI. It covers resolving config,
building the image, waiting for health, and why a step was skipped:

```
✓ seeding: skipped: db.files-api, s3.files-api still hold the data this seeded 2 hours ago
```

`--stop` shows the last stop or removal instead. When a start fails,
`events` is the first place to look: the failing step and its reason are
in the last lines.

For both commands, `<node>` is named as in [`fghj start`](/cli/nodes/), and
`--json` prints fghjd's JSON.
