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

## Not this folder: documentation for users

`concepts/` is written for someone *changing* fghj. Someone trying to **use**
it wants `docs/` instead — an Astro Starlight site, read either as plain
Markdown in the repo or locally with `cd docs && npm run dev`:

- `docs/src/content/docs/tutorial/` — a seven-chapter worked example, two
  repos taken from an empty directory to two concurrent runs. The intended
  entry point for a new user, and the one place the whole system is shown in
  order rather than by subsystem.
- `docs/src/content/docs/reference/fghj-yaml.md` — every field of
  `.fghj.yaml`.
- `docs/src/content/docs/concepts/` — a subset of this folder's guides,
  rewritten for a reader who has not read `src/`. These are *derived*: when a
  guide here changes, check whether its site counterpart still agrees. The
  file here stays the durable record.

The root `README.md` is the short version of all of it. See [[AUDIT]] §4.4
(D4) for what was wrong with the user-facing docs before this and what writing
them turned up about the code.

## Audit

[[AUDIT]] is a third kind of document: not a design decision, but a punch list.
It reads this whole folder against `schema/*.cue` and `src/` and asks whether
the language and capability surface are enough to describe an arbitrary local
dev environment — recording where they aren't, which invariants the docs assert
but nothing enforces, and which claims here have gone stale. Findings carry
stable ids (`E1`…, `B1`…, `D1`…, `R1`…) so they can be closed one at a time. When one
is closed, fold the resulting decision into the relevant concept file and mark
it in the audit's tracker — the concept files stay the durable record.

## Design decisions

| File | What it settles |
|---|---|
| [[flat-workspace-model]] | No repo is "root" — every repo is a peer, any repo can declare a `flow`, entry point is arbitrary. |
| [[branch-ownership-model]] | Branch identity lives on the one shared workspace checkout, never on a flow/dependency edge — a diamond dependency can't require two branches of the same repo at once. |
| [[fog-of-war-visibility]] | What's on the graph is driven by what's pulled to disk, not by flow membership — flows are a highlight layer, not a visibility filter. |
| [[terminating-nodes]] | Seeds and migrations are a third node kind that runs to completion, not a service with a clever restart policy — because "exited" means success for one and drift for the other. |
| [[language-boundaries]] | What `.fghj.yaml` deliberately cannot say — no host-process node kind, no branch on an edge — and how to tell a boundary from a missing knob. |
| [[build-inputs]] | A build may carry `target`, file secrets and the workspace owner's forwarded ssh-agent — but the credential path comes from `WorkspaceOwner`, never from the repo's own config, and BuildKit is used only when a build actually needs it. |

## Subsystem guides

| File | Covers |
|---|---|
| [[node-identity-and-domains]] | How a node gets its `id`/`label`, how that id becomes a `*.fghj.internal` domain, `domain_scope`, and named/primary ports. |
| [[local-ca-and-tls-proxy]] | The local root CA, on-the-fly per-SNI leaf certs, and the TLS-terminating reverse proxy that dispatches to real containers. |
| [[split-dns]] | The hand-rolled authoritative DNS server for `*.fghj.internal` and how it's wired into the OS resolver. |
| [[two-zones-and-raw-ports]] | Why there are two domain zones — `fghj.internal` (proxied, TLS, dispatch by name) and `fghj.raw.internal` (a real per-node address, any protocol) — and the virtual-IP allocator that makes the second one work. |
| [[in-network-sidecar]] | The one-container-per-run DNS+TLS proxy that makes `fghj.internal` names resolve the same way from inside a run's network as they do from the host. |
| [[host-aliases]] | `additional_hosts`/`wildcard_hosts` — literal hostnames fghj doesn't derive, the `/etc/hosts` block, and the one rule deciding what may ever get a certificate. |
| [[run-lifecycle-and-registry]] | Default vs. named/review runs, the reconciler, and how run state is kept honest against real Docker state. |
| [[state-and-effects]] | The actor/action/reducer/effects architecture: one store of state, one writer, and why an effect is `extract` + `converge`. |
| [[concurrency-model]] | What may run at the same time, what may not: per-node locks, merging writes, and the run-wide health budget. |
| [[config-drift]] | How fghj notices that a running container no longer matches the `.fghj.yaml` underneath it — and why it deliberately does nothing about it. |
| [[debugging-in-containers]] | `debug: <port>` publishes a port and injects nothing — the whole contract — plus why halting at startup is the one `FGHJ_DEBUG_WAIT` switch, and why it is deliberately absent from `spec_hash`. |
| [[persistence-and-workspace-store]] | The per-workspace SQLite store, the root-owned workspace index, and the root-runs-as-root/clones-as-you privilege split. |
| [[docker-and-downloads]] | Image builds, container lifecycle, and the background clone/pull job registry the UI polls. |
| [[control-api-and-cli]] | The axum control API, the `fghj`/`fghjd` process split, and the CLI's own hand-rolled HTTP client. |
| [[ui-architecture]] | The Svelte app's state model, the three tabs (Repos/Actual/Config), the graph layout algorithm, and the polling model that keeps it live. |

## Cross-cutting concerns

Neither one precept nor one subsystem: these describe a policy that every
part of the system has to answer, collected in one place so the answers can
be compared.

| File | Covers |
|---|---|
| [[config-language]] | CUE as an authoring aid vs. serde as the enforcing boundary, `Name`, what `version` means, and the process for adding to the language. |
| [[failure-semantics]] | What is fatal vs. advisory, what rolls back vs. what is kept, and the four places a failure actually surfaces. |
| [[security-model]] | The composite threat model: what adding a repo to your workspace grants its author, what fghj itself holds, and what is deliberately not defended against. |
| [[release-and-delivery]] | The four artifacts one version number keys together, why `ui/dist` is a compile-time prerequisite of every CI job, and which test suite CI structurally cannot run. |

## Map of the codebase

| Source | Guide |
|---|---|
| `src/resolver/` | [[node-identity-and-domains]], [[flat-workspace-model]], [[fog-of-war-visibility]], [[branch-ownership-model]] |
| `src/resolver/name.rs`, `src/resolver/version.rs`, `src/resolver/validate.rs` | [[config-language]] |
| `src/resolver/warning.rs`, `src/resolver/workspace_scan.rs` | [[failure-semantics]] |
| `src/web/ca.rs`, `src/web/proxy.rs` | [[local-ca-and-tls-proxy]] |
| `src/dns.rs` | [[split-dns]], [[two-zones-and-raw-ports]] |
| `src/raw_net/` | [[two-zones-and-raw-ports]] |
| `src/sidecar_image.rs`, `src/bin/fghj-sidecar.rs` | [[in-network-sidecar]], [[release-and-delivery]] |
| `src/actor.rs`, `src/action.rs`, `src/reducer/`, `src/state/`, `src/effects/` | [[state-and-effects]] |
| `src/runs/` | [[run-lifecycle-and-registry]], [[concurrency-model]], [[node-identity-and-domains]], [[terminating-nodes]] |
| `src/persistence/` | [[persistence-and-workspace-store]], [[security-model]] |
| `src/hosts_file.rs`, `src/effects/hosts.rs` | [[host-aliases]] |
| `src/runs/spec.rs`, `src/state/sync_status.rs` | [[config-drift]] |
| `src/runs/node_spec.rs`, `src/runs/start_node.rs` (`debug_wait_overrides`) | [[debugging-in-containers]], [[config-drift]] |
| `src/docker.rs`, `src/downloads.rs` | [[docker-and-downloads]], [[build-inputs]] |
| `src/web/api/`, `src/daemon/`, `src/daemon_log.rs`, `src/main.rs`, `src/bin/fghjd.rs` | [[control-api-and-cli]] |
| `Justfile`, `.github/workflows/`, `sidecar/Dockerfile`, `.dockerignore` | [[release-and-delivery]], [[in-network-sidecar]] |
| `src/web/ui.rs`, `ui/src/**` | [[ui-architecture]] |
| `schema/*.cue` | [[config-language]]; referenced throughout — an authoring aid for `.fghj.yaml` (editors, agents, CI, `fghj validate`), **not** what the daemon trusts. The Rust types that deserialize it are the enforcing boundary; the schema's obligation is only to never accept something the daemon would reject, which `resolver::name`'s drift test checks by reading these files |
