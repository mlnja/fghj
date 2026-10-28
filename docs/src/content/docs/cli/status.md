---
title: fghj status
description: Every node fghj knows about in a workspace, its state, and where it answers.
---

```bash
fghj status [--workspace <path>] [--json]
```

One row per node that has a container record:

```
NODE                  STATE      URL                                        RAW
api.files-api         running    https://api.files-api.files.fghj.internal  api.files-api.files.fghj.raw.internal:3000,9229
db.files-api          running                                               db.files-api.files.fghj.raw.internal:5432
migrate-db.files-api  completed
web.files-web         stopping…
```

- **STATE** is the node's condition, the same word the UI shows. While fghj
  is acting on a node, the action shows instead, such as `starting…` or
  `stopping…`.
- **URL** is where the proxy serves the node's primary port.
- **RAW** is the node's [raw address](/guides/networking-http-vs-raw/) and
  the ports that answer on it, each on its own number.

Only a running node shows addresses.

In a workspace where nothing has run yet, `status` lists the flows that
[`fghj up`](/cli/up/) can start instead.

`--json` prints fghjd's environment as is: each node's desired and observed
state, its derived `condition`, and any `pending_action`. Use it in scripts;
the text columns may change.
