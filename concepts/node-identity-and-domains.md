# Node identity and domain derivation

## The problem

Every node in the resolved graph — a service built from a repo, or a backing
dependency like postgres — eventually needs three distinct things, and it's
easy to accidentally conflate them:

1. **An id** — a stable key used internally (map keys, override targets,
   container names, edges) that must never collide between two unrelated
   nodes.
2. **A label** — what a human reads on the graph (`GraphView.svelte`'s node
   card, `Drawer.svelte`'s heading). Friendly, short, author-chosen.
3. **A domain** — the actual `*.fghj.internal` hostname a browser or another
   container reaches it at. Must be derivable without any author input, or
   two CUE authors who never talk to each other could hand-pick the same one.

`fghj.yaml` only ever declares the label (`#Service.name`, `#BackingDependency.name`).
Everything else — id and domain — is derived by fghj itself, in `src/resolver.rs`
and `src/runs.rs`.

## Why the id can't just be the label

Under [[flat-workspace-model]], any repo can be a peer with no ownership
relation to any other repo. Two departments can each maintain their own
service named `bff`, in their own repos, and never know about each other.
If `node.id` were just `component.service.name`, the second `bff` pulled
into the workspace would silently collide with — and, depending on map
insertion order, potentially overwrite — the first one's node. The same
problem exists one level down: two different services can each declare
their own backing dependency named e.g. `s3`, and a naive `owner.name`-style
id doesn't obviously prevent that either (it does, actually — see below —
but the *ordering* of the two components matters, which is the actual bug
this section fixes).

## The leaf-first, always-qualified convention

Every node id is a dotted chain, leaf (the specific thing) first, its owning
scope after:

- **Service**: `{service.name}.{repo's workspace folder name}` — e.g.
  `bff.dept-a-repo`. Built in `resolver::visit_local_service`. The folder
  name (`local_path`) is guaranteed unique because `scan_workspace` can't
  have produced two components under the same folder — it's a real
  directory listing. This qualification is applied **unconditionally**, not
  only when a collision is actually detected: if it were conditional, adding
  a second same-named peer repo later would change the *first* one's id
  retroactively (or silently rehost its domain), which is far worse than
  always paying the slightly longer id.
- **Backing dependency**: `{dep.name}.{owner's node.id}` — e.g.
  `s3.bff.dept-a-repo`. Built in `resolver::visit_dependency`'s
  `Dependency::Backing` arm. This was flipped from an earlier
  `{owner}.{dep.name}` ordering specifically to match the same leaf-first
  convention already used by named ports (below) — the specific resource
  comes first, its owning scope after, all the way down.
- **Shared-backing reference**: `#SharedBackingDependency` (the CUE shape a
  service uses to bind to *another* service's already-declared backing
  dependency, rather than provisioning a second instance) identifies the
  owning service by `repo` — the same way `#GitDependency` does — instead of
  by that service's declared `#Service.name`. It has to: the owning
  service's *name* alone isn't unique across peer repos anymore, but its
  `repo` URL is a portable, unambiguous identifier regardless of which
  workspace it's cloned into. `visit_dependency`'s `Dependency::SharedBacking`
  arm resolves `repo` → `local_path` (via `repo_index`, falling back to the
  URL's last path segment for not-yet-cloned repos) → `target_owner_id`,
  using exactly the same `{service.name}.{local_path}` formula
  `visit_local_service` used when it actually registered that node — so the
  two computations can never drift apart and produce a dangling reference by
  accident. A reference that genuinely doesn't resolve to a known backing
  node is still caught, as a non-fatal warning (`resolve_universe`'s
  "dangling shared-backing reference" pass), since a stub (not-yet-pulled)
  repo can't be checked yet.
- **Named port**: `{port.name}.{node's own domain}` — see below; the pattern
  repeats one more level down, at the port granularity.

`node.label` stays the bare CUE-declared name (`component.service.name`, or
`dep.name`) throughout — it's what the UI shows, and it's fine for it to
collide with a peer's, the way two people can share a first name.

## `#Port`: a port's role travels with the port

`#Service.ports` is a map, `{[port]: #Port}`, where `#Port` is
`{primary: bool | *false, name?: string}` (`resolver::PortConfig`) — not a
plain list of port numbers plus a separate list of "which ports are HTTP
routes" to keep in sync. This makes an entire class of bug structurally
impossible: a route naming a port the service never declared. The
resolver's `check_ports` only has one thing left to warn about — more than
one port claiming `primary` (only one port can occupy the node's own domain;
`check_ports` pushes a non-fatal warning, doesn't reject).

- `primary: true` puts that port at the node's own domain
  (`cart.myworkspace.fghj.internal`).
- `name: "admin"` gives that port an *additional* nested domain,
  `admin.cart.myworkspace.fghj.internal`. A port can be both `primary` and
  `name`d at once.
- Neither: the port is still published to an ephemeral localhost port by
  Docker, just with no `*.fghj.internal` name — reachable only by raw port
  number, never by name.

This is what lets a service with more than one HTTP surface (e.g.
Prometheus's scrape port plus its admin UI) expose both under sensible
names without any extra schema.

## Domain derivation: one formula, no exceptions

No node kind can declare its own raw domain — there is no
`#Service.internal_domain` (removed), no per-route domain override
(`#HttpRoute.domain` became `#HttpRoute.name`, a bare label). Every node's
domain, services included, is derived the same way by `runs::derive_domain`:

```rust
pub fn derive_domain(node_id: &str, domain_scope: &str, workspace_name: &str, run_id: &str) -> String {
    let workspace = sanitize_label(workspace_name);
    if domain_scope == "stable" || run_id == DEFAULT_RUN_ID {
        format!("{node_id}.{workspace}.fghj.internal")
    } else {
        format!("{node_id}.{run_id}.{workspace}.fghj.internal")
    }
}
```

`run_id` is folded in for named/review runs, since more than one can be
alive at once and each needs its own identity — but the **default run**
(the one shared per-workspace environment every "Start default environment"
click and every `ensure_running` targets) drops it, so a service's everyday
URL is just `cart.myworkspace.fghj.internal`, not
`cart.default.myworkspace.fghj.internal`. `container_name`/the Docker
network name still always fold in `run_id`, including for the default run —
this opt-out is domain-only.

The other opt-out is per-node, not per-run: `domain_scope: *"run" | "stable"`
(`#BackingDependency.domain_scope`, `#Service.domain_scope`). `"stable"`
drops the run id for that one node regardless of which run it's in — a
deliberate CUE-author choice (e.g. a postgres meant to keep one fixed
identity across every run of the graph), not an implicit bypass. Only one
run can actually own a `"stable"`-scoped name from the host at a time, but
it's always the same name.

`start_node` calls `derive_domain` when it actually launches a container,
and uses the result as **the sole Docker network alias** registered for that
container — so the name resolves identically whether asked from inside the
run's own docker network (via Docker's embedded per-network DNS) or from the
host (via `fghjd`'s own DNS server, which answers anything in the zone; see
[[split-dns]]). Named ports get their own alias the same way:
`{name}.{domain}` is pushed onto the same alias list, closing what used to
be a gap where a named port resolved from the host but not from sibling
containers.

## `Node.domain`: the default-run address, known ahead of time

Because the domain formula only depends on `node.id` + `domain_scope` +
workspace name (all known at resolve time) once `run_id` is fixed to
`DEFAULT_RUN_ID`, `resolve_universe` can — and does — pre-compute each
node's *default-run* domain and attach it as `Node.domain`, as a final pass
over the sorted node list, before any container for that node has ever
started:

```rust
node.domain = crate::runs::derive_domain(&node.id, &node.domain_scope, &workspace_name, crate::runs::DEFAULT_RUN_ID);
```

This is what `Drawer.svelte`'s "domain" info row and its "open" link
(`liveInfo.routes?.some(r => r.domain === node.domain)`, falling back to a
raw `127.0.0.1:<port>` link when no route matches) and `GraphView.svelte`'s
container-mode node card (`n.domain || n.image || ''`) both key off. Until
this field existed, that whole code path was silently dead — see the
"Fixed" entry in `PROGRESS.md` for the story.

**Caveat**: `Node.domain` always reflects the *default*-run address. A
review/named run gets a different, run-id-qualified domain (unless the node
opted into `domain_scope: "stable"`) that this field does not track — so the
"open via domain" link only lights up while the drawer is showing the
default run's live containers; for a named run it falls back to the port
link, and the domain row shown is the node's default identity, not that
run's actual one. Surfacing a run-scoped domain would need the frontend to
ask for (or the backend to attach) a domain scoped to whichever run is
currently selected, not the graph-wide default.

## Status

Implemented: `resolver::visit_local_service`/`visit_dependency` (ids),
`resolver::PortConfig`/`check_ports` (ports), `runs::derive_domain` (domain
formula, shared by `runs::start_node` and `resolver::resolve_universe`),
`Node.domain` (default-run address, pre-computed). See `PROGRESS.md` for the
session history behind the leaf-first id flip and the `Node.domain` fix.
