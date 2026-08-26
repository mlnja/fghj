---
title: fghj validate
description: Validate an fghj.yaml file against fghj's CUE schema.
---

```bash
fghj validate <path>
```

Checks a single `fghj.yaml` file against fghj's schema
(see [fghj.yaml](/reference/fghj-yaml/)) and reports whether it's a valid
component config.

## Arguments

| Argument | Description |
|---|---|
| `path` | Path to the `fghj.yaml` file to validate. |

## How it works

`validate` shells out to the [`cue`](https://cuelang.org/docs/install/)
CLI (`cue vet`), evaluating your file against fghj's built-in
`#ComponentConfig` schema — the same schema `dependency.cue` and
`component.cue` define, compiled into the `fghj` binary at build time. You
don't need to have those schema files locally; `fghj` writes them to a
temp directory before invoking `cue`.

`cue` must be installed and on your `PATH` — see
[Installation](/getting-started/installation/).

## Exit status

- Success: prints `<path> is a valid component config` and exits `0`.
- Failure: prints `cue`'s validation errors to stderr and exits non-zero.

This doesn't touch `fghjd`, Docker, or the network at all — it's a pure,
local schema check, safe to run in CI before a repo is ever wired into a
workspace.
