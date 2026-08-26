---
title: Quickstart
description: Wire your first workspace, browse the resolved graph, and run a service over a real HTTPS domain.
---

This walks through wiring a repo into a running `fghjd`, then starting it.
It assumes `fghjd` is already running (see [Installation](/getting-started/installation/))
and that the repo you're pointing at has an `fghj.yaml` — see
[fghj.yaml](/reference/fghj-yaml/) if you're setting one up for the first
time.

## 1. Validate a config

Before wiring anything in, you can check a service's `fghj.yaml` against
fghj's schema on its own:

```bash
fghj validate ./fghj.yaml
```

This shells out to `cue vet` under the hood — see
[fghj validate](/cli/validate/).

## 2. Wire a workspace

```bash
fghj wire git@github.com:acme/checkout-service.git
```

`wire` clones the entry repo (if it isn't already checked out alongside
your current directory), asks `fghjd` to resolve its full dependency graph,
and registers the workspace so it shows up in the UI. If the repo — or any
of its dependencies — declares Git-over-SSH dependencies, `fghj wire` is
what captures your SSH agent socket so the root-owned daemon can clone
them on your behalf; see
[Persistence & workspace store](/concepts/persistence-and-workspace-store/)
for why that hand-off exists.

## 3. Open the UI

`fghjd` serves a small web UI at `https://fghj.internal/` (scoped to the
workspace you just wired, or pick it from the workspace switcher if you
have more than one). Three tabs:

- **Repos** — the full dependency graph, as declared, regardless of what's
  actually running.
- **Actual** — the same graph overlaid with live container status, plus
  controls to start/stop runs.
- **Config** — reserved for environment/DNS/cert inspection; not built out
  yet.

See [UI architecture](/concepts/ui-architecture/) for the full tour.

## 4. Pull the rest of the graph

A freshly wired workspace only has the entry repo on disk — every
dependency it declares shows up as a not-yet-downloaded node. Click
**Pull all** in the header, or from the CLI's perspective, the daemon walks
the graph, clones anything missing, re-resolves, and repeats until nothing
new turns up (a repo's own dependencies aren't known until *it's* cloned).

## 5. Start the default environment

From the **Actual** tab, **Start default environment** brings up every
node currently on disk. This is the one shared environment for the
workspace — running it again after pulling more of the graph only starts
whatever's newly reachable, and never restarts what's already up. See
[Run lifecycle & registry](/concepts/run-lifecycle-and-registry/).

## 6. Open a service over HTTPS

Once a service's container is running, click its node for its resolved
domain — something like `checkout-service.acme-checkout.fghj.internal` —
and open it directly. The certificate is issued on the fly by fghj's local
CA and is already trusted, because `fghjd` installed that CA into your
system trust store on first start. No `-k`, no self-signed warning. See
[Local CA & TLS proxy](/concepts/local-ca-and-tls-proxy/) for how that
works, and [Node identity & domains](/concepts/node-identity-and-domains/)
for exactly how that domain was derived.

## Testing a specific branch

Need to test your checkout service against a teammate's in-progress branch
of the payments service, without disturbing the shared default
environment? Start a **named run** with a branch override from the
**Actual** tab's run controls instead of touching the default run — it
builds a disposable, throwaway checkout of that branch alongside the real
one. See [Run lifecycle & registry](/concepts/run-lifecycle-and-registry/)
and [Branch ownership model](/concepts/branch-ownership-model/) for why
this is the only way to pin a dependency to a specific branch.
