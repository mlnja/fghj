# Node identity and domain derivation

## The problem

Every node in the resolved graph — a service built from a repo, or one run
from an image, like postgres — eventually needs three distinct things, and it's
easy to accidentally conflate them:

1. **An id** — a stable key used internally (map keys, container names,
   edges) that must never collide between two unrelated nodes.
2. **A label** — what a human reads on the graph (`GraphView.svelte`'s node
   card, `Drawer.svelte`'s heading). Friendly, short, author-chosen.
3. **A domain** — the actual `*.fghj.internal` hostname a browser or another
   container reaches it at. Must be derivable without any author input, or
   two CUE authors who never talk to each other could hand-pick the same one.

`.fghj.yaml` only ever declares the label: the service's key under `services:` (`#Service`).
Everything else — id and domain — is derived by fghj itself, in `src/resolver/`
and `src/orchestrator/`.

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
  `bff.dept-a-repo`. Built by `resolver::visit::node_id`, called from
  `add_repo_nodes` for every service a scanned repo declares. A database is
  no different: Postgres is a service in some repo (`db.files-api`), not a
  dependency of one. The folder
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
- **A reference to another repo's service** — `depends_on: platform/cache`,
  `${FGHJ_SERVICE_FQDN:platform/redis}` — names it by the include alias the
  referring repo gave that repo, never by its folder.
  `reference_target_id` resolves the alias through that repo's `include:`
  map to the folder it lands in, then applies the same `node_id` formula
  `add_repo_nodes` used when that repo's services were registered — so the
  two computations can never drift apart and produce a dangling reference
  by accident. A bare name is a service of the referring repo itself. A
  reference that doesn't resolve is a blocking warning (`check_references`),
  except into a repo that's still a stub (not pulled yet), which can't be
  checked until it is.
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
domain, services included, is derived the same way by `orchestrator::derive_domain`:

```rust
pub fn derive_domain(node_id: &str, workspace_name: &str, zone: DomainZone) -> String {
    format!(
        "{node_id}.{}.{}",
        sanitize_label(workspace_name),
        zone.suffix()            // "fghj.internal" | "fghj.raw.internal"
    )
}
```

Every node has a name in **two** zones, and this one function derives both —
the zone is the only thing that differs. `fghj.internal` is the proxied,
TLS-terminated, dispatch-by-name address; `fghj.raw.internal` is the direct
one, for callers that need a real port. [[two-zones-and-raw-ports]] covers
what each is for and who answers it; everything below applies to both, since
the suffixes are disjoint.

A workspace has one environment, so a node has exactly one address per zone:
`cart.myworkspace.fghj.internal`. There used to be a run id folded in for
*named runs* (`cart.review.myworkspace.fghj.internal`) and a per-node
`domain_scope: "stable"` opt-out of it; both went with named runs
([[run-lifecycle-and-registry]]). Container and network names still carry
the fixed `DEFAULT_RUN_ID` segment, so existing containers keep their names
— that is a Docker-name detail, not part of any address.

`start_node` calls `derive_domain` when it actually launches a container, once
per zone. The **raw** name is the Docker network alias registered on the
container, so Docker's own embedded DNS resolves it to that container's IP for
free. The **http** name is deliberately *not* an alias: the run's sidecar owns
resolving it from inside the network ([[in-network-sidecar]]), so an
in-network lookup and a host-side lookup agree on the shape of the answer — an
address that terminates TLS and dispatches by name — even though the addresses
differ. Named ports get their own alias the same way: `{name}.{raw_domain}` is
pushed onto the same alias list, closing what used to be a gap where a named
port resolved from the host but not from sibling containers.

## The projection table

`node.id` is not a name anybody uses. It is the argument to eight different
naming functions, each of which produces a string in a *different* namespace,
with its own escaping rule and its own collision domain. Almost every naming
bug in this system has been a disagreement between two rows of this table, so
here is the table.

`W` = `sanitize_label(workspace folder name)`, `R` = the fixed
`DEFAULT_RUN_ID` (`default`), `Z` = the zone suffix (`fghj.internal` or `fghj.raw.internal`).

| Projection | Formula | Escaping | Namespace it shares | Who catches a collision |
|---|---|---|---|---|
| **Domain** (both zones) | `{id}.{W}.{Z}` — `orchestrator::derive_domain` | none: the id's dots become DNS label separators | one flat DNS zone, across *every* wired workspace | `resolver::uniqueness` (warning) for in-workspace clashes; `daemon::registry` (hard error) refuses to wire a second workspace whose folder sanitizes the same |
| **Named-port alias** | `{port.name}.{domain}` | none | the same DNS zone | `resolver::uniqueness` — this is the projection that can silently shadow a backing dependency's domain |
| **`additional_hosts` / `wildcard_hosts`** | author-written, verbatim | none | the same DNS zone, plus whatever real domain the author typed | `resolver::uniqueness` for the in-zone part; nothing at all if the author claims a name they don't own |
| **Container name** | `fghj-{W}-{R}-{sanitize_label(id)}` — `orchestrator::node_spec` | `sanitize_label`: every non-alphanumeric run collapses to one `-` | the Docker daemon, machine-wide — shared with every other workspace *and* with containers fghj didn't create | Docker itself, by refusing a duplicate name — with an error that never says "naming" |
| **Network name** | `fghj-{W}-{R}` — `orchestrator::orchestrate` | `sanitize_label` on the workspace only | the Docker daemon, machine-wide | Docker; but this one is *meant* to be idempotent (`ensure_network` reuses an existing one), so a clash between two workspaces that sanitize alike silently merges their networks — the same case `daemon::registry` already refuses to wire |
| **Volume name** | `fghj-vol-{sanitize_label(derive_domain({name}.{owner_id}, …))}` — `orchestrator::naming::derive_volume_name`; `shared: true` drops the `.{owner_id}` | `sanitize_label` over the whole derived domain | the Docker daemon, machine-wide | nothing — a volume collision is *silent*, which is exactly why the owner qualification is unconditional (see [[docker-and-downloads]]) |
| **Image tag** | `fghj/{sanitize_label(id)}:{sanitize_label(branch or "local")}` — `orchestrator::node_spec` | `sanitize_label` on both halves | the local image store, machine-wide | nothing; and note the tag is deliberately stable across rebuilds, which is why `build` had to be folded into `spec_hash` instead ([[build-inputs]]) |
| **Virtual IP** | `127.223.0.0/16` + SHA-256 of the raw-zone domain, then made sticky and collision-resolved by `raw_net::assign` | n/a — the name is hashed, not escaped | one /16 on the host | `raw_net::assign`, which probes forward on a real collision; see [[two-zones-and-raw-ports]] |

Three things fall out of reading it as a table rather than one row at a time.

**The escaping rules are not the same, and the divergence is the bug.** The
domain keeps dots; everything Docker-facing collapses them. Two rows derived
from one id can therefore be distinct in one namespace and identical in
another — which is the concrete failure the next section opens with.

**The collision domains are not the same either.** The DNS rows collide within
one zone across all wired workspaces. The Docker rows collide across the whole
machine, including with containers, networks and volumes fghj has never heard
of. Only the virtual-IP row has a collision domain fghj fully owns, which is
why it's the only one that can resolve a collision by moving rather than by
refusing.

**Only some rows have an enforcer.** Domains and aliases are checked, container
names are checked by Docker for the wrong reason, and volume and image names
are not checked at all. That asymmetry is the argument for `resolver::name`
below: the cheapest place to make all eight rows safe is *before* the id
exists, by restricting the alphabet to the intersection every row accepts.

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

## `Node.domain`: the address, known ahead of time

Because the domain formula only depends on `node.id` and the workspace name
(both known at resolve time), `resolve_universe` can — and does —
pre-compute each node's domain and attach it as `Node.domain`, as a final
pass over the sorted node list, before any container for that node has ever
started:

```rust
node.domain = crate::orchestrator::derive_domain(&node.id, &workspace_name, DomainZone::Http);
```

This is what `Drawer.svelte`'s "domain" info row and its "open" link
(`liveInfo.routes?.some(r => r.domain === node.domain)`, falling back to a
raw `127.0.0.1:<port>` link when no route matches) and `GraphView.svelte`'s
container-mode node card (`n.domain || n.image || ''`) both key off. Until
this field existed, that whole code path was silently dead.

## Related

- [[two-zones-and-raw-ports]] — the second half of the domain story: what the
  `zone` parameter is for, and who answers each name.
- [[in-network-sidecar]] — why the http-zone name is not a Docker alias.
- [[docker-and-downloads]] — the volume row of the projection table, in full.
- [[flat-workspace-model]] — why ids must be qualified at all.

## Status

Implemented: `resolver::visit::node_id` and `reference_target_id` (ids),
`resolver::PortConfig`/`check_ports` (ports), `orchestrator::derive_domain` (domain
formula, shared by `orchestrator::start_node` and `resolver::resolve_universe`),
`orchestrator::naming::derive_volume_name` (volumes), `orchestrator::node_spec` (container
names, image tags), `raw_net` (virtual IPs), `resolver::uniqueness` (the
collision pass), `resolver::name::Name` (the alphabet).
