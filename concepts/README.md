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

## Audit

[[AUDIT]] is a third kind of document: not a design decision, but a punch list.
It reads this whole folder against `schema/*.cue` and `src/` and asks whether
the language and capability surface are enough to describe an arbitrary local
dev environment — recording where they aren't, which invariants the docs assert
but nothing enforces, and which claims here have gone stale. Findings carry
stable ids (`E1`…, `B1`…, `D1`…) so they can be closed one at a time. When one
is closed, fold the resulting decision into the relevant concept file and mark
it in the audit's tracker — the concept files stay the durable record.

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
| [[run-lifecycle-and-registry]] | Default vs. named/review runs, the reconciler, and how run state is kept honest against real Docker state. |
| [[state-and-effects]] | The actor/action/reducer/effects architecture: one store of state, one writer, and why an effect is `extract` + `converge`. |
| [[concurrency-model]] | What may run at the same time, what may not: per-node locks, merging writes, and the run-wide health budget. |
| [[persistence-and-workspace-store]] | The per-workspace SQLite store, the root-owned workspace index, and the root-runs-as-root/clones-as-you privilege split. |
| [[docker-and-downloads]] | Image builds, container lifecycle, and the background clone/pull job registry the UI polls. |
| [[control-api-and-cli]] | The axum control API, the `fghj`/`fghjd` process split, and the CLI's own hand-rolled HTTP client. |
| [[ui-architecture]] | The Svelte app's state model, the three tabs (Repos/Actual/Config), the graph layout algorithm, and the polling model that keeps it live. |

## Map of the codebase

| Source | Guide |
|---|---|
| `src/resolver/` | [[node-identity-and-domains]], [[flat-workspace-model]], [[fog-of-war-visibility]], [[branch-ownership-model]] |
| `src/web/ca.rs`, `src/web/proxy.rs` | [[local-ca-and-tls-proxy]] |
| `src/dns.rs` | [[split-dns]] |
| `src/actor.rs`, `src/action.rs`, `src/reducer/`, `src/state/`, `src/effects/` | [[state-and-effects]] |
| `src/runs/` | [[run-lifecycle-and-registry]], [[concurrency-model]], [[node-identity-and-domains]] |
| `src/persistence/` | [[persistence-and-workspace-store]] |
| `src/docker.rs`, `src/downloads.rs` | [[docker-and-downloads]] |
| `src/web/api/`, `src/daemon/`, `src/main.rs`, `src/bin/fghjd.rs` | [[control-api-and-cli]] |
| `src/web/ui.rs`, `ui/src/**` | [[ui-architecture]] |
| `schema/*.cue` | referenced throughout — an authoring aid for `.fghj.yaml` (editors, agents, CI, `fghj validate`), **not** what the daemon trusts. The Rust types that deserialize it are the enforcing boundary; the schema's obligation is only to never accept something the daemon would reject, which `resolver::name`'s drift test checks by reading these files |
