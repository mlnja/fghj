---
title: Branch ownership model
description: Flow membership, live branch state, and per-run branch pins are three distinct things with three distinct owners.
---

Three things get confused if you're not careful, so `fghj` gives them three
distinct owners:

1. **Flow membership** — *which repos* are in scope for a user journey.
   This is repo-owned, versioned config: whichever repo's `.fghj.yaml`
   declares the flow owns its `dependencies` list (see
   [Flat workspace model](/concepts/flat-workspace-model/)). A
   dependency's `default_branch` lives here too, but it's only ever a
   *default* — the initial clone target, and the fallback build tag. It's
   never a live pin.
2. **Live branch/dirty state** — *which branch a repo is on right now*.
   This is owned by the workspace checkout: one real Git working tree per
   repo, shared by every flow that happens to reference it. There's no
   per-flow copy of a dependency's checkout, so there's exactly one branch
   active per repo, workspace-wide, at any moment.
3. **Per-run branch pin** — this used to exist (a `RunSpec` override
   building from a throwaway mirror + checkout under `.fghj/`, never
   touching the live workspace checkout or config) but has been **removed
   entirely**: its privilege-dropped mirror clone was never actually sound,
   and it had no answer for an override branch whose own `.fghj.yaml`
   structurally differs from the live graph. See
   [Run lifecycle & registry](/concepts/run-lifecycle-and-registry/) for
   the removal note. There is currently no per-run branch pin mechanism.

## Why this matters

Because branch identity lives on the checkout — a singleton per repo — and
not on a dependency edge, **a diamond dependency can never require two
different branches of the same repo at once**. That failure mode is
structurally impossible, not just avoided by convention: two flows (or two
dependents) referencing the same repo are necessarily looking at the same
checkout, on the same branch, because there's only one checkout. (There's
currently no supported way to build one dependency from a different branch
for a single experiment without checking it out live — see the removal
note above.)

A repo can legitimately be a `main`-default dependency of one flow and a
`develop`-default dependency of another; only one branch is ever actually
checked out, and that's fine — `default_branch` disagreeing across two
edges was never a real conflict, since it's only a clone-time default.

This doesn't make dependency *cycles* impossible — a flow-scoped
dependency in one direction can coexist with a hard dependency in the
other direction. That's a real cycle in the requirement graph, just not a
branch conflict. [UI architecture](/concepts/ui-architecture/) covers how
the graph layout handles that case visually.

## fghj is a passive observer, not a branch manager

There's no command or UI action to switch a repo's branch. You switch
branches the normal way — `git checkout`/`git switch` directly in the
workspace — and `fghj` just reflects that: the Repos tab polls the graph
every few seconds and shows each checkout's live branch and a dirty/clean
indicator, purely as a read-only reflection of real Git state. `fghj`
never mutates a checkout itself.

There's also no separate "flow" node on the graph — a flow is a named lens
over the one shared repo graph, not a node in it or a view beside it.
Selecting a flow in the header highlights that flow's nodes and edges;
every known repo still renders regardless of which flow is selected, per
[Fog-of-war visibility](/concepts/fog-of-war-visibility/).
