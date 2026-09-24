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
convention: the last path segment of the repo URL, `.git` stripped. There is
no override — `local_path` was removed from the schema, and nothing replaced
it.

That makes the repo→folder map non-injective: `github.com/org-a/api` and
`github.com/org-b/api` both want `<ws>/api`, and host and org are discarded.
fghj cannot hold both, and the honest consequence is that **pulling the second
one fails loudly** — `downloads::ensure_checkout_is` compares the existing
checkout's `origin` against the requested URL and refuses a mismatch. It used
to report success and leave the resolver reading the first repo's `.fghj.yaml`
as though it were the second's. A flat namespace that rejects a collision is a
limitation; one that silently resolves it the wrong way is a bug.
[[node-identity-and-domains]] rests its whole uniqueness argument on this
folder name, so it is worth being clear about which of the two it is.

## Lazy, partial resolution

Resolving the graph never blocks on a repo that isn't cloned yet. A
dependency whose local folder isn't present in the workspace renders as a
`downloaded: false` stub node instead of failing — you can see it, see what
flow it belongs to, and see it's not there yet.

"Pull all" clones every missing repo into the workspace by convention,
recursively, until nothing new appears — a fixpoint loop, since a
newly-cloned repo might declare its *own* flows or dependencies nobody could
see before it existed on disk.

A repo that *is* on disk but whose `.fghj.yaml` cannot be read is a third
case, and it is neither of the first two. `scan_workspace` skips it and
reports it rather than aborting the scan: one unparseable file used to 500
the graph endpoint for the entire workspace, which meant the UI could not
render the very workspace you needed in order to find the bad file.

It is skipped, not stubbed, and the distinction is the whole design. A
missing repo stubs because fghj genuinely does not know what it declares —
`downloaded: false` is an honest statement of ignorance. A malformed repo is
right there, saying something fghj cannot parse; treating it as "declares
nothing" would substitute a guess for the author's intent without saying so.
So it becomes a blocking warning: the workspace stays viewable, and starts
refuse until it is fixed.

## Status

Implemented in `fghj` (see `src/resolver.rs`: `scan_workspace`,
`resolve_universe`, `pull_all`; `Node.downloaded`). Plan history:
`/Users/virviil/.claude/plans/fluffy-cooking-ripple.md`.

This file is a running index — drop other "fancy shit" design ideas here as
separate files (or sections) as they come up, whether or not they're
implemented yet.
