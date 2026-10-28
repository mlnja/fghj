---
title: fghj pull
description: Clone the repos a workspace references but doesn't have yet.
---

```bash
fghj pull [<node> | --flow <flow>] [--workspace <path>] [--json]
```

Clones the repos the workspace needs and doesn't have yet:

- with `<node>`, the repos that one node needs;
- with `--flow`, the repos that flow needs;
- with neither, every repo the workspace references.

Repos already checked out are left alone. `pull` never runs `git pull` on
an existing checkout.

It prints each clone as it goes and exits non-zero if any failed. Clones
use your own git credentials, as [`fghj wire`](/cli/wire/) does.
