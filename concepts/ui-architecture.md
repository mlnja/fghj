# UI architecture

## One page, three tabs over one shared graph

The whole UI is a single Svelte 5 app (`App.svelte`, runes-based state —
`$state`/`$derived`/`$effect`) with no client-side router: `activeTab`
(`'repos' | 'containers' | 'config'`) just switches which body renders under
a fixed `Header`. All three tabs read from the same resolved graph
(`universe`, from `GET /universe.json`) — they're different *views* over one
shared model, not three separately-fetched datasets:

- **Repos** (`reposGraph`): one node per repo (nodes grouped by
  `local_path`), with cross-repo edges merged from `depends-on` and `uses` —
  `depends-on` wins when both connect the same pair. "Who requires whom,"
  matching [[flat-workspace-model]]/[[fog-of-war-visibility]]'s
  repo-centric graph. Backed by `GraphView` in `mode="repos"`.
- **Actual** (`containersGraph`): every node and edge — "everything the
  daemon would eventually run," services, images and tasks together,
  overlaid with live container status via `runContainers` (a
  `node_id → ContainerInfo` map derived from whichever run is currently
  selected). Backed by `GraphView` in `mode="containers"`, plus
  `RunControls` for starting/stopping runs.

Layout uses `depends-on` edges only, since they're the start order. Drawn:
`depends-on` solid, optional (`required: false`) dashed, `uses` (a hostname
reference, which orders nothing) dotted and fainter.
- **Config**: an explicit placeholder — env vars, split-DNS table, issued
  certs — none of that has any real UI surface yet, so the tab says so
  outright rather than showing an empty shell that looks broken.

A flow selection (`currentFlow`, from the flow picker in `Header`) is a
highlight, never a filter, on every tab — see [[fog-of-war-visibility]] and
[[branch-ownership-model]] for why: every known repo always renders, dimmed
when it's outside the selected flow.

## Multi-workspace from one page

`currentWorkspaceId` is read from `?workspace=` in the URL or
`localStorage`, and every fetch (`withWs`) appends it as a query param —
this is the frontend half of `WorkspaceExtractor` on the backend (see
[[control-api-and-cli]]). Switching workspaces (`selectWorkspace`) resets
every piece of workspace-scoped state (`universe`, `selectedNode`, `runs`,
`selectedRunId`) before re-fetching, so stale data from the previous
workspace never briefly renders under the new one's identity.

## Polling, not push

There's no websocket/SSE for the graph or run list — both are plain
interval polling, gated by which tab is active so an inactive tab doesn't
waste requests:

- `/universe.json` every 3s while the Repos tab is active (`App.svelte`'s
  `graphPoll` effect) — reflects live git state (branch/dirty — see
  [[branch-ownership-model]]) without a manual refresh.
- `/runs` every 1s while the Actual tab is active (`runsPoll`) — matched to
  `daemon::spawn_reconciler`'s own 1s tick (see
  [[run-lifecycle-and-registry]]), so the UI is essentially never stale
  relative to what the backend has already reconciled.

Logs are the one exception: `Drawer.svelte` opens a real `EventSource`
against `/runs/{run}/nodes/{node}/logs/stream` (SSE, backed by
`docker::logs_follow`) whenever the Logs tab is open on a node with a live
container — genuinely pushed, not polled, since log lines arriving late by
even a second would be a much more noticeable regression than a graph node
being briefly stale.

Both `pull`/`pull-all`/`pull-flow` and run-start/stop follow the same
"kick off, then poll a status endpoint" shape client-side (`Header.svelte`'s
`pullAll`/`pullFlow`, `Drawer.svelte`'s `download`) — matching
[[docker-and-downloads]]'s background-job design on the backend: the POST
returns immediately, and a short-interval poll (600ms) against `.../status`
drives the spinner/checkmark until the job leaves `"running"`.

## `GraphView`'s layout algorithm

`GraphView.svelte`'s `layout()` runs a longest-path layered layout, computed
fresh from `graph.nodes`/`graph.edges` on every render (`$derived`):

1. **Cycle removal.** A real cross-flow dependency cycle is legitimate under
   this model (see [[branch-ownership-model]]'s note on flow-scoped vs.
   hard dependencies pointing in opposite directions) — but a longest-path
   depth assignment over a graph with a cycle doesn't terminate. A DFS pass
   classifies edges as tree/forward vs. back-edges (an edge into a node
   still `in-progress` on the current DFS stack) and drops only the
   back-edges before the depth pass runs, so the *visual* layout is always
   a DAG even when the underlying dependency graph legitimately isn't one.
2. **Depth assignment.** A relaxation loop over the (now acyclic) edge set
   pushes each node's depth to `max(depth[parent] + 1)`, bounded at
   `node_count + 1` iterations as a safety guard.
3. **Deterministic ordering.** Nodes are sorted by `id` (not by insertion
   order) before being bucketed into depth-levels, specifically so the
   layout doesn't depend on `currentFlow` or on the raw order the backend's
   JSON happened to list nodes in — switching which flow is highlighted
   never moves a single node, only recolors it. This mirrors
   `resolve_universe`'s own choice to sort `nodes` by id before serializing
   (see [[fog-of-war-visibility]]'s Status section) — both exist to stop the
   graph from visibly jittering on every poll for reasons unrelated to any
   real change.

## The four drawers

- **`Drawer`** (node detail): general info + logs, opened by clicking any
  node. Shows the node's default-run domain and (when live) its container
  status/name and an "open" link — see [[node-identity-and-domains]]'s
  `Node.domain` section for exactly which run's address that link reflects
  and why it doesn't always resolve to the currently-selected run.
- **`OperationsDrawer`** (the "☰" queue view): lists every download job
  ever started this daemon lifetime (`DownloadRegistry::list`, most-recent
  first) with its live log — the one place to see a `pull-all` or
  individual clone's raw git output.
- **`TelemetryDrawer`** (the "⚡" header button): the daemon's own view of
  itself, not any workspace's — three tabs. `Logs` (the `daemon_log` ring
  buffer); `DNS / DNAT`, listing the managed `/etc/hosts` block, the
  `/etc/resolver` zones with the ephemeral port each points at, and the live
  `pf` NAT routes backing the raw zone; and `Doctor`, the same report
  `fghj doctor` prints. Together these answer "is the plumbing actually up"
  without reading `fghjd`'s stdout — see [[split-dns]], [[host-aliases]],
  [[two-zones-and-raw-ports]], [[preflight-checks]].

  The first two poll (1.5s and 2s); `Doctor` deliberately does not. It
  fetches on tab open and then only from its own re-run button, because every
  check shells out to the OS (`ifconfig`, `getaddrinfo`,
  `security verify-cert`, three TCP connects) and nothing it measures changes
  on its own — a missing loopback alias does not come back by itself. The
  interaction a doctor wants is read, go fix, re-run, which is a button, not
  a timer.
- **`SideDrawer`**: not a feature on its own — a shared shell (fixed-width
  panel sliding in from the right, click-outside-to-close) all three of the
  above render their actual content into via Svelte's `{@render children()}`
  snippet mechanism.

## The design system, and the docs site that borrows it

`ui/src/app.css` is the whole design system: one palette (`--bg`, `--panel`,
`--ink*`, one `--accent`, and a success/warning/danger trio), three faces
(Podkova for display, Space Grotesk for body, JetBrains Mono for every piece
of chrome text), a 4px spacing scale and three radii. Every component style in
`ui/src/**` is written against those variables and never against a literal
colour, which is what lets the whole app be re-skinned from one file.

The documentation site is painted from the same system. `docs/` is Astro
Starlight, which has its own `--sl-*` variable vocabulary, so the two are
joined by a projection rather than by sharing a file:
`docs/src/styles/fghj-tokens.css` restates the palette and maps it onto the
`--sl-*` names, and `docs/src/styles/fghj-theme.css` rebuilds the handful of
*shapes* that make the app recognisable — the eyebrow (tiny uppercase mono
label with an accent dot) over a Podkova heading, mono chrome text, hairline
panels on the canvas, and the corner brackets from `.corner-brackets`.

Two things are worth knowing before changing either side:

- **The hex values are duplicated, deliberately.** The docs build is a
  separate npm project with no import path into `ui/`, and a shared file
  would have to be generated into both. The cost is that a palette change
  has to be made twice; `fghj-tokens.css` says so at the top and names
  `ui/src/app.css` as the source of truth.
- **The docs have a dark theme and the app does not.** Starlight ships a
  theme toggle, so the dark half of `fghj-tokens.css` is *derived* — the same
  ink hue taken to the background end of the ramp, with the accent lifted
  until it carries text contrast. It is not a claim about what a dark fghj UI
  would look like. Three places where the projection is not one-to-one, all
  recorded in the files themselves: the app's `.stencil` is uppercase and the
  docs' page titles are not (half of them are literal strings like
  `.fghj.yaml`, which upper-casing would falsify), Starlight's `:::tip`
  purple is remapped onto the success green because fghj has no purple, and
  Starlight's per-card accent rotation is pinned to the one accent.

## Status

Implemented: the full `ui/src/**` tree as described. Not implemented: the
Config tab (explicit placeholder only — no env var / split-DNS / cert
surface exists yet, per `SPEC.md`'s Subsystem B/C once they grow that far).
