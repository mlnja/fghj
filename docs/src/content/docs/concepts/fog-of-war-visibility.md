---
title: Fog-of-war visibility
description: What's on the graph is driven by what's pulled to disk, not by flow membership — flows are a highlight, never a filter.
---

## The idea

What you *see* on the graph is driven by what's on disk (pulled), not by
flow membership. Flows are a highlight layer on top of the map, not a
visibility filter — like fog of war in a game: you see the whole area
you've explored, and quests (flows) light up the parts of it that are
relevant right now.

Concretely: a repo you've pulled into the workspace shows up on the graph
— with its own declared infra/service dependencies — even if it declares
no `flows` of its own, and no other repo's flow happens to reference it.
It renders dimmed, not hidden, when it isn't part of whichever flow is
currently selected in the picker.

## Why it works this way

Under [Flat workspace model](/concepts/flat-workspace-model/), any repo can
be a starting point — you might `fghj wire` straight into a service that
declares no flows and that nothing else references yet. If visibility were
driven by flow membership, that repo (and its infra) would be invisible on
the graph despite sitting right there on disk. Fog-of-war visibility keeps
"what's pulled" and "what's relevant to this journey" as two separate
questions: the resolver first walks every declared flow, then makes a
second pass over everything `scan_workspace` actually found on disk and
adds anything not already visited — with an empty `flows: []` — so it
still renders, just unhighlighted.

## Status

Implemented in the resolver's `resolve_universe`: after walking every
declared flow, a second pass visits every repo found on disk
unconditionally. [UI architecture](/concepts/ui-architecture/) covers how
`GraphView` uses each node's `flows` list purely for opacity, never as a
filter.
