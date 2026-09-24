# Node identity and domain derivation

## The problem

Every node in the resolved graph — a service built from a repo, or a backing
dependency like postgres — eventually needs three distinct things, and it's
easy to accidentally conflate them:

1. **An id** — a stable key used internally (map keys, container names,
   edges) that must never collide between two unrelated nodes.
2. **A label** — what a human reads on the graph (`GraphView.svelte`'s node
   card, `Drawer.svelte`'s heading). Friendly, short, author-chosen.
3. **A domain** — the actual `*.fghj.internal` hostname a browser or another
   container reaches it at. Must be derivable without any author input, or
   two CUE authors who never talk to each other could hand-pick the same one.

`.fghj.yaml` only ever declares the label (`#Service.name`, `#BackingDependency.name`).
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
  name is unique because `scan_workspace` can't have produced two components
  under the same folder — it's a real directory listing. Note what that does
  and doesn't buy: uniqueness holds because a colliding repo *cannot be
  present*, not because the repo→folder map is injective. It isn't — org and
  host are dropped — so `org-a/api` and `org-b/api` compete for one folder and
  the second one to be pulled is refused (see [[flat-workspace-model]]). This qualification is applied **unconditionally**, not
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
run can actually *serve* a `"stable"`-scoped name from the host at a time, but
it's always the same name — and note that nothing enforces the "only one":
both runs register the route, and `resolve_route` returns whichever comes
first in map order. Which run you reach is unspecified.

`start_node` calls `derive_domain` when it actually launches a container,
and uses the result as **the sole Docker network alias** registered for that
container — so the name resolves identically whether asked from inside the
run's own docker network (via Docker's embedded per-network DNS) or from the
host (via `fghjd`'s own DNS server, which answers anything in the zone; see
[[split-dns]]). Named ports get their own alias the same way:
`{name}.{domain}` is pushed onto the same alias list, closing what used to
be a gap where a named port resolved from the host but not from sibling
containers.

## Unique ids do not make unique names

Everything above is about making `node.id` injective. The names *derived*
from it are a separate question, and the answer is no: uniqueness does not
survive the projections.

Two distinct ways it breaks, and they fail differently.

**Concatenation without a discriminator.** A backing dependency's domain is
`{dep.name}.{owner_domain}`; a named port's alias is `{port.name}.{domain}`.
Same shape, built by two independent pieces of code, landing in one flat DNS
codomain. A service `cart` with a backing dependency named `minio` *and* a
port named `minio` claims `minio.cart.shop.<ws>.fghj.internal` twice — one
network alias, one DNS name, one cert, one route. `state::query::resolve_route`
is a `find_map` over a `BTreeMap`: first match by key order wins, silently.
Nothing about the formula is wrong; the codomain is just shared.

**Escaping that destroys the separator.** `derive_domain` keeps the id's dots.
`container_name` runs the same id through `sanitize_label`, which collapses
every non-alphanumeric run to `-`. Service names may contain hyphens, so `a-b.c`
and `a.b.c` — two genuinely different nodes with two different domains — both
become `a-b-c`. Docker refuses the duplicate name and the second node fails to
start, with an error that never mentions naming. The two projections of one id
have opposite escaping rules, which is the whole bug.

`resolver::uniqueness::check_derived_name_collisions` warns on both, running at
the end of `resolve_universe` (it needs `Node.domain`, so it can't live in the
traversal). It reconstructs the codomain `resolve_route` actually scans — node
domains, named-port aliases, `additional_hosts`, `wildcard_hosts` — and reports
any name claimed twice along with *what* made each node claim it, which is the
part that can't be guessed from the string.

One more namespace with the same shape, checked elsewhere because the resolver
can't see it: the workspace segment is `sanitize_label(folder_name)`, while
`resolve_route` walks *every* wired workspace. `~/work/shop` and
`~/scratch/shop` derive byte-identical domains. `daemon::registry` refuses to
wire the second one — a hard error, not a warning, because after both are wired
there is nothing left to disambiguate with.

## What a name is, and who enforces it

Every argument above assumes names are well-behaved: that a service name can go
into a node id, a derived domain, and a Docker network alias without anything in
between having to escape it. For a long time nothing checked that. The CUE said
`=~"^[a-z0-9][a-z0-9-]*$"`, but CUE is enforced only by `fghj validate` — an
opt-in command shelling out to an external binary that may not be installed, and
which the daemon never runs. The real boundary was serde, and serde was strictly
more permissive.

So a service called `My Service!` parsed fine, flowed unsanitized into
`node.id`, and from there into a DNS name and a network alias that are simply
invalid — while the *container* name, which goes through `sanitize_label`,
worked. One unvalidated input reaching three namespaces with three different
escaping disciplines.

`resolver::name::Name` is where that stops. It is a newtype whose `Deserialize`
enforces the pattern, applied to every field that reaches an id: service map
keys, flow `service`, backing `name`, a `kind: service` dependency's `services:`
list, shared-backing `service`/`name`, port `name`, volume `name`. There is no
`From<&str>` — the only ways to build one are `Deserialize` and `Name::parse`,
both of which validate — so holding a `Name` is proof it is safe in all three
namespaces unescaped.

That last part is why the pattern is what it is: it is the *intersection* of
what a DNS label, a network alias and a container name accept, not the union.
Nothing downstream needs to escape, so no two escapings can disagree.

### CUE is for the author, not the daemon

The schema is a convenience for whoever is writing the file — their editor,
their agent, their CI, `fghj validate`. It is not what the daemon trusts, and
the daemon does not consult it. Enforcing it at load would mean a `cue`
subprocess per `.fghj.yaml` on every resolve (and the daemon re-resolves on a
poll), plus a hard runtime dependency on a binary most machines do not have.

Which leaves the schema one obligation: **never accept what the daemon would
reject.** A file that passes `fghj validate` and then fails to resolve is the
schema lying to its only audience. `rust_and_cue_agree_on_what_a_name_is` holds
it to that by reading `schema/*.cue`, pulling out every `=~"…"` identifier
constraint, and failing if any of them differs from the Rust pattern — a checked
fact rather than a comment asking people to remember.

An invalid name makes its repo unreadable, which lands as a blocking warning
naming the repo, file and line while every other repo resolves normally (see
[[flat-workspace-model]]). That is the right severity: a name *is* the identity,
so a service whose name is invalid has no id to be represented by.

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
