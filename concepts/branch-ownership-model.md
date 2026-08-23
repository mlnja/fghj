# Branch ownership: flows never pin a branch, checkouts do

## The idea

Three distinct things get confused if you're not careful, so they get three
distinct owners:

1. **Flow membership** — *which repos* are in scope for a user journey. This
   is repo-owned, versioned config: whichever repo's `fghj.yaml` declares the
   flow owns its `dependencies` list, per [[flat-workspace-model]]. A
   dependency's `default_branch` lives here too, but it is only ever a
   *default* — the initial `pull_all` clone target, and the fallback label
   for a run's build tag. It is never a live pin.
2. **Live branch/dirty state** — *which branch a repo is on right now*. This
   is owned by the **workspace checkout**: one real git working tree per
   repo, shared by every flow that happens to reference it. There is no
   per-flow copy of a dependency's checkout, so there is exactly one branch
   active per repo, workspace-wide, at any moment.
3. **Per-run branch pin** — an ephemeral override scoped to a single run,
   already implemented as `RunSpec.overrides` (`src/runs.rs`). It builds from
   a throwaway mirror+checkout under `<workspace>/.fghj/` and never touches
   the live workspace checkout or any config.

## Why this matters

Because branch identity lives on the checkout (a singleton per repo) and not
on a dependency edge, **a diamond dependency can never require two different
branches of the same repo at once** — that failure mode is structurally
impossible here, not just avoided by convention. Two flows (or two
dependents) referencing the same repo are necessarily looking at the same
checkout, on the same branch, because there is only one checkout. If you
want a specific dependency built from a specific branch for one experiment,
that's what the per-run override exists for — it's explicitly ephemeral and
side-by-side with the live checkout, not a second "real" branch state for
that repo.

`resolver.rs` used to track a `path_branches: HashMap<repo, HashSet<branch>>`
and flag a `"diamond conflict"` warning (plus a `conflict` field on `Node`/
`Edge`) whenever two dependency edges declared different `default_branch`
values for the same repo. That detector has been **deleted entirely**, not
just left dormant — it was checking a property (declared `default_branch`
per edge) that this model says is only ever a clone-time default, never a
live pin, so two edges disagreeing about it was never actually a conflict to
begin with. A repo can legitimately be a `main`-default dependency of one
flow and a `develop`-default dependency of another; only one branch is ever
actually checked out, and that's fine.

Note this doesn't make dependency cycles impossible — a flow-scoped
dependency in one direction (e.g. `auth-service`'s `signup-flow` depending on
`notification-service`) can coexist with a hard dependency in the other
direction (`notification-service`'s existing dependency on `auth-service`).
That's a real cycle in the requirement graph, just not a branch conflict —
see the layout note under Status.

## fghj is a passive observer, not a branch manager

There is no `fghj branch` / `branch set` command and no UI action to switch
a repo's branch. You switch branches the normal way — `git checkout`/
`git switch` directly in `<workspace>/<local_path>` — and fghj just reflects
that. The existing "Repos" tab (the flow-dependency graph, `mode="repos"` in
`ui/src/lib/GraphView.svelte`) already draws one card per on-disk checkout,
so live branch/dirty state is folded directly into that same card rather
than living in a separate view: each node's branch label
(`node-meta.branch-row`) sits next to a DIRTY/CLEAN pill, and the node
detail drawer shows the same status. The graph is polled (every 3s while
the tab is active, `App.svelte`) purely so this reflects live git state
without a manual refresh — fghj never mutates a checkout itself.

There is also no separate "flow" node/rectangle on the graph, and no
separate tab for it — a flow is a named lens over the one shared repo graph,
not a node in it or a view beside it. Selecting a flow in the header picker
highlights that flow's nodes/edges (accent border, brighter accent stroke)
via each `Node`/`Edge`'s `flows: string[]` field; every known repo still
renders regardless of which flow is selected, per
[[fog-of-war-visibility]]. Graph layout (`GraphView.svelte`'s `layout()`) is
computed once from the full node/edge set alone — it does not depend on
`currentFlow` at all, so switching flows never moves a single box, only
recolors it.

## Status

Implemented: `resolver::git_status_dirty` and `Node.dirty`
(`src/resolver.rs`, set in `visit_local_service` for every real on-disk
checkout; always `false` for stub/infra nodes, which have no checkout) —
surfaced in `GraphView.svelte`'s node cards and `Drawer.svelte`'s detail
rows, with `App.svelte` polling `/universe.json` every 3s while the
flow-graph ("Repos") tab is active.

Also implemented, as fallout from building this: `resolve_universe` sorts
its returned `nodes` by id before serializing, since `ctx.nodes` is a
`HashMap` and would otherwise hand back a different order on every poll —
this used to make the graph visibly reshuffle every ~3s independent of any
user action. `GraphView.svelte`'s `layout()` also sorts defensively and
drops cycle-forming back-edges (via DFS) before its longest-path depth pass,
since a real cross-flow dependency cycle (see above) would otherwise make
that pass's depth grow without bound and blow out the canvas width.
