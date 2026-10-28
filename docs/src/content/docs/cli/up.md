---
title: fghj up
description: Switch a workspace to a flow from the terminal and wait until it is up.
---

```bash
fghj up <flow> [--workspace <path>] [--json]
```

Switches the workspace to a flow, the same switch as picking the flow in
the UI. Nodes outside the flow stop, the flow's nodes start, and `up`
prints fghjd's log of the switch as it goes. It returns when the switch has
settled and prints the result as [`fghj status`](/cli/status/) does.

`<flow>` is `repo/flow`, or just the flow's name when only one repo has a
flow by that name:

```bash
fghj up demo             # files-api/demo
fghj up files-web/full
```

A name that matches nothing, or more than one flow, fails and lists the
flows that do exist.

It exits non-zero if the switch failed. The log above the error says which
node failed and why; [`fghj events <node>`](/cli/logs/#fghj-events) has the
step-by-step detail.

`Ctrl-C` stops the watching, not the switch: fghjd carries on, and
`fghj status` shows where it got to.

| Option | Description |
|---|---|
| `--workspace <path>` | The workspace root. By default it's [the wired workspace you're standing in](/cli/exec/#the-workspace-is-derived-from-where-you-are). |
| `--json` | Print nothing while the switch runs, then the environment as fghjd's JSON. |
