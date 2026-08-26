---
title: Architecture
description: How fghj's pieces — the CLI, the superdaemon, the resolver, the proxy, and the UI — fit together.
---

`fghj` is two binaries and a bundled web UI:

```
                     Developer laptop / browser
                                │
                 Request to *.fghj.internal (HTTPS)
                                │
                                ▼
          ┌─────────────────────────────────────────────┐
          │                  fghjd                        │
          │   (root — DNS, TLS proxy, CA, control API)    │
          └───────────────────┬─────────────────────────┘
                               │
              ┌────────────────┼────────────────┐
              ▼                ▼                ▼
        DNS (:ephemeral)  TLS proxy (:443)  Control API (:ephemeral)
        *.fghj.internal   SNI-routed          + embedded UI
                                │
                                ▼
                          Docker containers
                     (one per resolved node)
```

- **`fghjd`** is the root-owned superdaemon — one instance per machine,
  modeled on `dockerd`. It owns everything that needs a privileged port or
  system trust changes: the DNS server for `*.fghj.internal`, the
  TLS-terminating reverse proxy on 80/443, the local certificate authority,
  and Docker access. See [Local CA & TLS proxy](/concepts/local-ca-and-tls-proxy/),
  [Split DNS](/concepts/split-dns/), and [Docker & downloads](/concepts/docker-and-downloads/).
- **`fghj`** is the CLI a developer runs directly — `validate`, `graph`,
  `wire`, `daemon stop`. It's unprivileged and never touches Docker or ports
  80/443 itself; everything that needs root goes through `fghjd`'s HTTP
  control API. See [Control API](/concepts/control-api/).
- **The resolver** (`src/resolver.rs`) turns a workspace of sibling repo
  checkouts, each with its own `fghj.yaml`, into one resolved graph of
  nodes and edges — assigning stable ids, `*.fghj.internal` domains, and
  flow membership along the way. See [Node identity & domains](/concepts/node-identity-and-domains/),
  [Flat workspace model](/concepts/flat-workspace-model/), and
  [Fog-of-war visibility](/concepts/fog-of-war-visibility/).
- **The run registry** (`src/runs.rs`) turns that graph into running Docker
  containers — one shared default environment per workspace, plus
  disposable named runs for testing a specific branch. See
  [Run lifecycle & registry](/concepts/run-lifecycle-and-registry/) and
  [Branch ownership model](/concepts/branch-ownership-model/).
- **The web UI** (Svelte, embedded into `fghjd` at compile time) is how you
  browse the graph, start/stop runs, and tail logs. See
  [UI architecture](/concepts/ui-architecture/).
- **Persistence** (`src/store.rs`) is split into a root-owned index of
  known workspaces and a per-workspace SQLite database for run/container
  state — see [Persistence & workspace store](/concepts/persistence-and-workspace-store/).

Each page in this section is a self-contained guide to one of those pieces
— what problem it solves, how it's built, and the non-obvious reasoning
behind specific choices.
