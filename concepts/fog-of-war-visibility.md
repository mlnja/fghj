# Fog of war: pulled repos are always on the map, flows are a highlight

## The idea

What you *see* is driven by what's on disk (pulled), not by flow membership.
Flows are a highlight/overlay layer on top of the map, not a visibility
filter — like fog of war in a game: you see the whole area you've explored,
and quests (flows) light up the parts of it that are relevant right now.

Concretely: a repo you've pulled into the workspace shows up on the graph —
with its own declared infra/service dependencies — even if:
- it declares no `flows` of its own, and
- no other repo's flow happens to reference it.

It just renders dimmed/unhighlighted (not hidden) when it isn't part of
whatever flow is currently selected in the picker.

## Why this matters

Before this, `resolve_universe` only ever registered a node by walking
outward from a flow's `dependencies` — so an entry repo with no flows (e.g.
you `fghj ui`'d straight into `cart-service` and haven't pulled anything that
references it) produced an *empty* graph, even though the repo and its infra
were sitting right there on disk. That contradicted the whole point of
[[flat-workspace-model]]: any repo can be a starting point, and what's pulled
should always be visible regardless of whether a flow happens to reach it.

## Status

Implemented in `src/resolver.rs::resolve_universe` — after walking every
declared flow, a second pass calls `visit_local_service` for every repo
`scan_workspace` found on disk, unconditionally. Nodes visited only this way
get `flows: []` and render dimmed in `GraphView.svelte` (which already only
uses `flows` for opacity, never as a filter) instead of not existing at all.
