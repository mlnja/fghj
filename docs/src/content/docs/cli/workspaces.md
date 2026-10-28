---
title: fghj workspaces
description: List the wired workspaces, or unwire one.
---

```bash
fghj workspaces [--json]
fghj workspaces rm <id>
```

Lists every workspace wired into fghjd, with its id and root directory.

`rm` unwires a workspace. Its containers, sidecar and network are removed;
its checkouts and volumes stay. The workspace leaves the list at once, and
`rm` returns once its containers are gone. To use it again, run
[`fghj wire`](/cli/wire/).
