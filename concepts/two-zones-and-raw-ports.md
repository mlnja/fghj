# Two zones: `fghj.internal` and `fghj.raw.internal`

> **Status:** implemented. Zones in `src/dns/` (`ZONE`, `ZONE_RAW`,
> `ZoneSource`) and `src/orchestrator/domain.rs` (`DomainZone`, `derive_domain`);
> host-side answering in `src/daemon/routing.rs`; in-network answering in
> `src/bin/fghj-sidecar.rs` (see [[in-network-sidecar]]); virtual IPs and NAT
> in `src/raw_net/`; templating in `src/orchestrator/fqdn_template.rs`. Closes
> [[AUDIT]] D1's stale `derive_domain` signature and D2's dual-zone,
> `raw_net` and FQDN-templating gaps.

## Why one zone was not enough

[[split-dns]] describes a single authoritative zone, `fghj.internal`, where
**every** name resolves to the same address and a TLS reverse proxy
dispatches by SNI once the connection lands. That works because HTTP carries
the hostname inside the request, so one address can serve every service.

Raw TCP does not. A Postgres client connecting to
`db.myworkspace.fghj.internal:5432` sends no hostname — the proxy has nothing
to dispatch on. One shared address can multiplex HTTP by name and cannot
multiplex anything else at all.

So there are two zones, and which one a name is in **is** the statement of how
it can be reached:

| | `fghj.internal` | `fghj.raw.internal` |
|---|---|---|
| resolves to | one shared address | a per-node address |
| dispatch | by SNI/Host, in the proxy | none — it *is* the node |
| TLS | terminated by fghj's CA | never |
| certificates | eligible | never (`cert_eligible` matches `ZONE` only) |
| port | 443 (or 80) | the node's own declared port |
| in-network | answered by the run's sidecar | answered by Docker's embedded DNS |
| from the host | always `127.222.0.1` | a `raw_net` virtual IP, NAT'd |

The two are disjoint despite looking nested: `fghj.raw.internal` does not end
with `.fghj.internal`, so `dns::in_zone` cannot accidentally claim a raw name.

Every node gets a name in **both** zones, derived by the same function.
`ContainerDesired` carries both (`domain` and `raw_domain`), because a node
does not choose — the caller does, by choosing which name to use.

## `derive_domain`, with the zone

```rust
pub fn derive_domain(node_id: &str, workspace_name: &str, zone: DomainZone) -> String {
    format!(
        "{node_id}.{}.{}",
        sanitize_label(workspace_name),
        zone.suffix()            // "fghj.internal" | "fghj.raw.internal"
    )
}
```

The zone is the *only* thing that differs between a node's two names. Every
uniqueness argument in [[node-identity-and-domains]] therefore holds in both
zones at once, and holds *between* them trivially, since the suffixes differ.

`resolver::resolve_universe` calls this with `DomainZone::Http` only, so
`Node.domain` carries a node's browser address before any container exists. The raw name has no equivalent
pre-computed field on `Node` — it only becomes meaningful once there is a
container to alias it onto.

## Who answers what

Only one of these names is a real Docker network alias. `raw_domain` is
registered on the container itself, so Docker's own embedded DNS resolves it
to that container's IP for free, from anywhere on the run's network. `domain`
is deliberately **not** registered as an alias: the run's sidecar owns
resolving it, so an in-network lookup and a host-side lookup agree on the
shape of the answer (an address that terminates TLS and dispatches by name),
even though the actual address differs.

That gives four answer paths, two per zone:

- **host → `fghj.internal`** — `daemon::routing`'s `ZoneSource` answers
  `dns::ANSWER` (`127.222.0.1`), always, for any in-zone name. The host proxy
  dispatches by SNI.
- **host → `fghj.raw.internal`** — answered with `raw_net::resolve(qname)`, a
  per-name virtual IP. See below.
- **in-network → `fghj.internal`** — the sidecar answers with its *own*
  discovered IP for any name in its route table. See [[in-network-sidecar]].
- **in-network → `fghj.raw.internal`** — the sidecar does not answer; Docker's
  embedded DNS does, from the alias.

A name in neither zone is forwarded verbatim upstream, never NXDOMAIN'd — see
[[split-dns]].

## Host-reachable raw ports: virtual IPs and NAT

Inside the network, a raw name resolves to a container IP and the story ends.
From the *host* there is no such address — Docker publishes container ports to
`127.0.0.1:<some ephemeral port>`, which is reachable but has the wrong name
and an unpredictable port.

`raw_net` closes that gap without asking the user to look anything up: each
raw-zone name gets a **virtual IP** out of a `/16` fghjd owns exclusively
(`127.223.0.0/16`), DNS answers with it, and fghjd itself listens on
`virtual_ip:container_port` and copies the bytes to `127.0.0.1:host_port`. The result is that
`db.myworkspace.fghj.raw.internal:5432` works from the host with the port the
author declared, not the one Docker picked.

`127.223.0.0/16` sits inside loopback, next to the proxy's `127.222.0.1`. No
VPN, LAN or proxy can route `127/8`, so the pool can't shadow a real network
the way a `10/8` pool could on a corporate subnet. On Linux every
address in `127/8` is already local, so there's nothing to alias; macOS has
only `127.0.0.1` and gets one `lo0` alias per address in use. (Class E,
`240.0.0.0/8`, was tried first and rejected: BSD-derived stacks drop it even
when aliased.)

### The allocator is a hash, and then it isn't

`virtual_ip_for` is a pure SHA-256 of the name, folded into the pool. Pure
matters: the DNS answer path and the listener reconciler never coordinate, and they
must agree on the same address for the same name — a shared hash gets that for
free, and it survives a daemon restart unchanged.

A pure hash can also collide, and the module says so rather than waving it
away. A collision only *matters* when it lands two **active** names on the
same `(virtual_ip, container_port)` pair, since that pair is what a listener
binds — two names sharing an address but declaring different ports is
harmless. So `assign`/`pick_ip` layer a thin sticky table on top:

- an incumbent keeps its existing address unless one of its ports genuinely
  conflicts with another active name's claim this round;
- a name that must move probes forward from its naive hash until every one of
  its ports is free;
- incumbents are resolved before newcomers, each in sorted order, so a name
  never moves merely because a "better" slot opened up, and simultaneous
  arrivals resolve deterministically;
- an address another program on the machine already holds is a claim too:
  the bind fails with `AddrInUse`, `reconcile` records it, and the name moves
  on the next round.

The table is **in-memory only, deliberately not persisted**. Nothing about
correctness depends on matching a previous process's addresses — only on
`resolve` and `reconcile` agreeing while the daemon is up, which they do
because they read the same table.

A name with no running container still resolves, to its naive hash address.
That is consistent with the http zone's "always answer in-zone" behaviour: the
answer is just an address nothing listens on yet.

### fghjd listens; the kernel doesn't translate

`raw_net::forward` receives the *complete* desired route set every time,
never a diff, and makes its listeners match: new addresses get a socket, gone
ones are closed, and a route whose container moved to a new published port
keeps its listener and just points new connections at the new port. Each
connection is a plain two-way byte copy, so it works for any TCP protocol.

Kernel NAT would avoid the userspace hop, and it's the wrong trade. The
firewall is shared machine state that fghjd doesn't own: any other program
that reloads it can switch fghjd's rules off without a trace, while they
still read back as loaded. A socket can't be switched off from outside,
needs no firewall on any OS, and is the same mechanism the proxy already
uses for `127.222.0.1:443`. The hop costs nothing that matters for local
development.

## Naming a sibling without hardcoding the formula

A config author should never hand-compute `derive_domain`'s output into a
literal string, and before `fqdn_template` existed that is exactly what every
`*_HOST`/`*_URL` value in the real configs did.

```yaml
environment:
  DATABASE_URL: postgres://app@${FGHJ_SERVICE_FQDN:postgres}:5432/app
  PUBLIC_BASE:  https://${FGHJ_SERVICE_FQDN_HTTP}
```

The bare token resolves to the **raw** zone, and `_HTTP` is the opt-in for the
proxied one. That default is the right way round for the same reason the raw
zone exists: every real caller of this today is a connection string — a
database, an S3 endpoint — and those need a real port. The proxied identity is
the rare case (a presigned URL meant to leave the network).

`:name` names a sibling; bare means this node itself. The longer `_HTTP` token
is tested first so it is never read as the shorter one plus a literal suffix.
An unresolvable sibling or an unterminated token is left in the output rather
than failing the run — the same tolerance `parse_env_file` extends to a
malformed line, and a literal `${FGHJ_SERVICE_FQDN:typo}` in a container's env
is at least diagnosable.

This is the **only** templating the config language has. It is not a general
interpolation mechanism, and deliberately cannot read the host environment —
that is [[AUDIT]] E2, still open.

## Related

- [[split-dns]] — the DNS server itself, upstream forwarding, and how the OS
  resolver is pointed at it.
- [[in-network-sidecar]] — who answers `fghj.internal` from inside a run.
- [[node-identity-and-domains]] — where a node's id comes from, and the full
  table of projections a node id feeds.
- [[local-ca-and-tls-proxy]] — why only the http zone is certificate-eligible.
- [[state-and-effects]] — `raw_net` reconciliation as a fanned-in effect over
  `state::query::raw_endpoints`.
