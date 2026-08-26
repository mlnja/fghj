---
title: UI architecture
description: The Svelte app's state model, its three tabs, the graph layout algorithm, and the polling model that keeps it live.
---

## One page, three tabs over one shared graph

The UI is a single Svelte app with no client-side router — a tab selector
just switches which body renders under a fixed header. All three tabs
read from the same resolved graph — they're different *views* over one
shared model, not three separately fetched datasets:

- **Repos** — service nodes and their `depends-on` edges only: "who
  requires whom," matching the repo-centric graph described in
  [Flat workspace model](/concepts/flat-workspace-model/) and
  [Fog-of-war visibility](/concepts/fog-of-war-visibility/).
- **Actual** — every node kind, services and backing dependencies
  together, overlaid with live container status from whichever run is
  currently selected. This is also where run controls live — starting the
  default environment, running a specific flow, or starting a named run
  with a branch override.
- **Config** — reserved for environment variables, the split-DNS table,
  and issued certs; not built out yet.

A flow selection is a highlight, never a filter, on every tab — see
[Fog-of-war visibility](/concepts/fog-of-war-visibility/) and
[Branch ownership model](/concepts/branch-ownership-model/): every known
repo always renders, dimmed when it's outside the selected flow.

## Multi-workspace from one page

The current workspace is tracked in the URL and applied to every request
automatically, matching the control API's own workspace-scoping — see
[Control API](/concepts/control-api/). Switching workspaces resets every
piece of workspace-scoped state before re-fetching, so stale data from
the previous workspace never briefly renders under the new one's
identity.

## Polling, not push

There's no websocket for the graph or run list — both are plain interval
polling, gated by which tab is active so an inactive tab doesn't waste
requests: the graph refreshes every few seconds on the Repos tab
(reflecting live Git branch/dirty state without a manual refresh — see
[Branch ownership model](/concepts/branch-ownership-model/)), and the run
list refreshes on a faster interval on the Actual tab, matched to the
backend reconciler's own tick rate (see
[Run lifecycle & registry](/concepts/run-lifecycle-and-registry/)) so the
UI is essentially never stale relative to what the backend has already
reconciled.

Logs are the one exception: the log drawer opens a real live stream
whenever it's showing a running container — genuinely pushed, not polled,
since log lines arriving late would be a much more noticeable regression
than a graph node being briefly stale.

Both pull jobs and run start/stop follow the same "kick off, then poll a
status endpoint" shape client-side — matching the background-job design
described in [Docker & downloads](/concepts/docker-and-downloads/): the
request returns immediately, and a short-interval poll against a status
endpoint drives the spinner/checkmark until the job finishes.

## The graph's layout algorithm

The graph view runs a longest-path layered layout, computed fresh from
the current node/edge set on every render:

1. **Cycle removal.** A real cross-flow dependency cycle is legitimate
   under this model (see [Branch ownership model](/concepts/branch-ownership-model/)'s
   note on flow-scoped vs. hard dependencies pointing in opposite
   directions) — but a longest-path depth assignment over a graph with a
   cycle doesn't terminate. A depth-first pass classifies back-edges (an
   edge into a node still in progress on the current traversal) and drops
   only those before the depth pass runs, so the *visual* layout is
   always a DAG even when the underlying dependency graph legitimately
   isn't one.
2. **Depth assignment.** A relaxation pass over the now-acyclic edge set
   pushes each node's depth to one more than its deepest parent.
3. **Deterministic ordering.** Nodes are sorted by id, not by insertion
   order, before being bucketed into depth levels — so the layout doesn't
   depend on which flow is highlighted or on the raw order the graph
   happened to be served in. Switching which flow is highlighted never
   moves a single node, only recolors it — this mirrors the resolver's
   own choice to sort nodes by id before serializing the graph, so it
   doesn't visibly jitter on every poll for reasons unrelated to any real
   change.

## Drawers

- **Node detail** — general info and logs, opened by clicking any node.
  Shows the node's default-run domain and, when live, its container
  status and an "open" link — see
  [Node identity & domains](/concepts/node-identity-and-domains/) for
  exactly which run's address that link reflects.
- **Operations** — lists every download job started this daemon lifetime,
  most-recent-first, with its live log — the one place to see a "pull
  all" or an individual clone's raw Git output.
