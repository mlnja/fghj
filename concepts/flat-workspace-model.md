# Flat workspace model: peer repos, any-repo flows, lazy pull

## The idea

No repo is special. There is no "root"/"platform" repo that owns the flow
graph — every repo is a peer, and any repo can declare a `flows:` block for
a user journey it cares about (e.g. `auth-service` might declare a "simple
login flow", `cart-service` might declare a "checkout flow"). Entry point is
just whichever repo you happen to start `fghj` with — could be the backend,
could be the frontend. The only asymmetry that matters is dependency
direction (frontend has no reference to backend; backend references
frontend), not "importance."

All repos live as sibling folders in one flat **workspace** folder, named by
convention (last path segment of the repo URL, `.git` stripped) but
overridable via `local_path`.

## Lazy, partial resolution

Resolving the graph never blocks on a repo that isn't cloned yet. A
dependency whose local folder isn't present in the workspace renders as a
`downloaded: false` stub node instead of failing — you can see it, see what
flow it belongs to, and see it's not there yet.

"Pull all" clones every missing repo into the workspace by convention,
recursively, until nothing new appears — a fixpoint loop, since a
newly-cloned repo might declare its *own* flows or dependencies nobody could
see before it existed on disk.

## Status

Implemented in `fghj` (see `src/resolver.rs`: `scan_workspace`,
`resolve_universe`, `pull_all`; `Node.downloaded`). Plan history:
`/Users/virviil/.claude/plans/fluffy-cooking-ripple.md`.

This file is a running index — drop other "fancy shit" design ideas here as
separate files (or sections) as they come up, whether or not they're
implemented yet.
