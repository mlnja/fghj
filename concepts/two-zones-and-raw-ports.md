# Two zones: `fghj.internal` and `fghj.raw.internal`

> **Status:** implemented. Zones in `src/dns.rs` (`ZONE`, `ZONE_RAW`,
> `ZoneSource`) and `src/runs/domain.rs` (`DomainZone`, `derive_domain`);
> host-side answering in `src/daemon/routing.rs`; in-network answering in
> `src/bin/fghj-sidecar.rs` (see [[in-network-sidecar]]); virtual IPs and NAT
> in `src/raw_net/`; templating in `src/runs/fqdn_template.rs`. Closes
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
pub fn derive_domain(
    node_id: &str,
    domain_scope: &str,
    workspace_name: &str,
    run_id: &str,
    zone: DomainZone,
) -> String {
    let workspace = sanitize_label(workspace_name);
    let suffix = zone.suffix();          // "fghj.internal" | "fghj.raw.internal"
    if domain_scope == "stable" || run_id == DEFAULT_RUN_ID {
        format!("{node_id}.{workspace}.{suffix}")
    } else {
        format!("{node_id}.{run_id}.{workspace}.{suffix}")
    }
}
```

The zone is the *only* thing that differs between a node's two names. Every
uniqueness argument in [[node-identity-and-domains]] therefore holds in both
zones at once, and holds *between* them trivially, since the suffixes differ.

`resolver::resolve_universe` calls this with `DEFAULT_RUN_ID` and
`DomainZone::Http` only, so `Node.domain` carries a node's default-run browser
address before any container exists. The raw name has no equivalent
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
(`10.222.0.0/16`), DNS answers with it, and the OS NATs
`virtual_ip:container_port` → `127.0.0.1:host_port`. The result is that
`db.myworkspace.fghj.raw.internal:5432` works from the host with the port the
author declared, not the one Docker picked.

`10.222.0.0/16` is ordinary RFC 1918 space, chosen after `240.0.0.0/8`
(Class E) was tried and rejected — BSD-derived stacks, macOS's included, have
a real history of treating Class E as martian and silently dropping it even
when locally aliased. A dedicated `/16` also gives the backend a clean "this
entire range is mine" rule for pruning stale interface aliases.

### The allocator is a hash, and then it isn't

`virtual_ip_for` is a pure SHA-256 of the name, folded into the pool. Pure
matters: the DNS answer path and the NAT reconciler never coordinate, and they
must agree on the same address for the same name — a shared hash gets that for
free, and it survives a daemon restart unchanged.

A pure hash can also collide, and the module says so rather than waving it
away. A collision only *matters* when it lands two **active** names on the
same `(virtual_ip, container_port)` pair, since that pair is what a NAT rule
keys on — two names sharing an address but declaring different ports is
harmless. So `assign`/`pick_ip` layer a thin sticky table on top:

- an incumbent keeps its existing address unless one of its ports genuinely
  conflicts with another active name's claim this round;
- a name that must move probes forward from its naive hash until every one of
  its ports is free;
- incumbents are resolved before newcomers, each in sorted order, so a name
  never moves merely because a "better" slot opened up, and simultaneous
  arrivals resolve deterministically.

The table is **in-memory only, deliberately not persisted**. Nothing about
correctness depends on matching a previous process's addresses — only on
`resolve` and `reconcile` agreeing while the daemon is up, which they do
because they read the same table.

A name with no running container still resolves, to its naive hash address.
That is consistent with the http zone's "always answer in-zone" behaviour: the
answer is just an address nothing NATs to yet.

### The backend owns one pf anchor and nothing else

`RawNetBackend::apply` always receives the *complete* desired route set, never
a diff — the same whole-state-every-tick contract `hosts_file::sync` uses, so
each platform picks its own idempotency strategy. macOS gets `pf`/`ifconfig`;
every other platform gets a `NoopBackend` that warns once, so the crate
compiles everywhere and raw names still resolve (to an address nothing
answers on) rather than failing differently per OS.

The macOS backend's design is worth recording because two earlier attempts
failed in instructive ways, both documented in `src/raw_net/macos.rs`:

1. **Nesting under `com.apple/*`** — rules loaded two levels under stock
   macOS's wildcard `rdr-anchor` simply never fired, on a clean boot, and no
   amount of reloading or rebooting fixed it.
2. **Owning the top-level ruleset** — reading `/etc/pf.conf`, splicing our
   rules in, and reloading. This worked, and *silently erased Docker
   Desktop's own live-only NAT rules within a second of startup*, breaking
   every `127.0.0.1:<published-port>` on the machine. Docker injects those
   rules into the live kernel ruleset without ever writing them to disk, so a
   disk-based reload cannot preserve what it cannot see.

The hard rule that failure established, and that the current design exists to
honour: **fghjd must never be the one who breaks someone else's networking**,
even though someone else reloading pf is allowed to transiently break fghjd.

So the backend owns a dedicated top-level named anchor (`fghjd`). A one-time
idempotent edit adds a bare `rdr-anchor "fghjd"` hook to `/etc/pf.conf` — the
ordinary way third-party pf tools hook in — and every tick thereafter runs
only `pfctl -a fghjd -f -`, which can replace nothing but our own anchor's
contents. The hook line is removed on a clean `clear()` and is harmless if a
killed daemon leaves it: an anchor with nothing loaded is a no-op.

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
