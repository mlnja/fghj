# concepts/ — the fghj design guide

This folder is the durable design record for `fghj` (see `PROGRESS.md` for
the house convention it started from). Two kinds of document live here:

- **Design decisions** — short, focused write-ups of a single precept that
  shapes the system (why it's true, what it rules out, where it's
  implemented). This was the original convention: one file per idea, a
  `## Status` section, cross-linked via `[[other-concept]]`.
- **Subsystem guides** (this batch) — longer, end-to-end walkthroughs of one
  whole piece of the system: what problem it solves, how the pieces fit
  together, and the non-obvious reasons behind specific choices, in the
  style of a framework's own guide series (think Phoenix's guides, one per
  subsystem, meant to be read start to finish). Every non-trivial "why" that
  used to live only as a comment buried in the relevant `.rs`/`.svelte` file
  has been pulled into the matching guide below, so the reasoning survives
  a refactor that might otherwise delete the comment along with the code it
  explained.

Both kinds use the same `## Status` convention (implemented vs. aspirational,
and where) and the same `[[name]]` cross-link style. Treat this folder as
required reading before proposing an architecture change — check whether a
concept already covers the idea, or explains why current behavior is the way
it is, before re-deriving it from the code.

For the product vision this all serves, see `SPEC.md` (note: aspirational in
places — see the CLI-surface mismatch called out in
[[control-api-and-cli]] and in `PROGRESS.md`'s "Known gaps"). For a running
snapshot of what's actually built and what's left, see `PROGRESS.md` itself.

## Design decisions

| File | What it settles |
|---|---|
| [[flat-workspace-model]] | No repo is "root" — every repo is a peer, any repo can declare a `flow`, entry point is arbitrary. |
| [[branch-ownership-model]] | Branch identity lives on the one shared workspace checkout, never on a flow/dependency edge — a diamond dependency can't require two branches of the same repo at once. |
| [[fog-of-war-visibility]] | What's on the graph is driven by what's pulled to disk, not by flow membership — flows are a highlight layer, not a visibility filter. |

## Subsystem guides

| File | Covers |
|---|---|
| [[node-identity-and-domains]] | How a node gets its `id`/`label`, how that id becomes a `*.fghj.internal` domain, `domain_scope`, and named/primary ports. |
| [[local-ca-and-tls-proxy]] | The local root CA, on-the-fly per-SNI leaf certs, and the TLS-terminating reverse proxy that dispatches to real containers. |
| [[split-dns]] | The hand-rolled authoritative DNS server for `*.fghj.internal` and how it's wired into the OS resolver. |
| [[run-lifecycle-and-registry]] | Default vs. named/review runs, branch overrides, the reconciler, and how run state is kept honest against real Docker state. |
| [[persistence-and-workspace-store]] | The per-workspace SQLite store, the root-owned workspace index, and the root-runs-as-root/clones-as-you privilege split. |
| [[docker-and-downloads]] | Image builds, container lifecycle, and the background clone/pull job registry the UI polls. |
| [[control-api-and-cli]] | The axum control API, the `fghj`/`fghjd` process split, and the CLI's own hand-rolled HTTP client. |
| [[ui-architecture]] | The Svelte app's state model, the three tabs (Repos/Actual/Config), the graph layout algorithm, and the polling model that keeps it live. |

## Map of the codebase

| Source | Guide |
|---|---|
| `src/resolver.rs` | [[node-identity-and-domains]], [[flat-workspace-model]], [[fog-of-war-visibility]], [[branch-ownership-model]] |
| `src/ca.rs`, `src/proxy.rs` | [[local-ca-and-tls-proxy]] |
| `src/dns.rs` | [[split-dns]] |
| `src/runs.rs` | [[run-lifecycle-and-registry]], [[node-identity-and-domains]] |
| `src/store.rs` | [[persistence-and-workspace-store]] |
| `src/docker.rs`, `src/downloads.rs` | [[docker-and-downloads]] |
| `src/daemon.rs`, `src/main.rs`, `src/bin/fghjd.rs`, `src/server.rs` | [[control-api-and-cli]] |
| `ui/src/**` | [[ui-architecture]] |
| `schema/*.cue` | referenced throughout — the CUE shapes are the source of truth for `fghj.yaml`, cross-checked against the Rust structs that deserialize it in [[node-identity-and-domains]] |
