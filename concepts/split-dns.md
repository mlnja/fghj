# Split-DNS: a minimal authoritative server for `*.fghj.internal`

## The problem

For `https://cart.myworkspace.fghj.internal` to work in a browser, something
has to answer that `A` query with `127.0.0.1` — the OS's normal resolver
(talking to whatever DNS the network/VPN provides) has never heard of
`fghj.internal` and would just NXDOMAIN it. `fghj` needs its own
authoritative answer for exactly one zone, without touching resolution for
anything else on the machine.

## A hand-rolled server, on purpose

`src/dns.rs` implements the DNS wire format directly — `parse_query`/
`build_response` — rather than pulling in a general-purpose DNS server
crate. The zone is fixed (`ZONE = "fghj.internal"`), the answer is always
the same (`127.0.0.1`, TTL 5s — short, so a container restart's new
container name/route is picked up quickly rather than being cached stale on
the client), and every in-zone query gets that answer while everything
out-of-zone gets `NXDOMAIN`. `dns::in_zone` — checking a query name against
`ZONE_SUFFIX = ".fghj.internal"` (plus the bare apex) — is `pub(crate)` and
reused by `ca::DynamicCertResolver`, so "is this name ours" has exactly one
implementation shared between the two subsystems that both need to answer
it (see [[local-ca-and-tls-proxy]]).

## Why an ephemeral port, not the SPEC's suggested 5353

`SPEC.md` describes the DNS server listening on a fixed `127.0.0.1:5353`.
The actual implementation (`dns::bind`) instead binds whatever port the OS
hands out. `5353` is mDNS's well-known port, and on a real dev Mac it's
routinely already bound by `mDNSResponder`/Chrome — a fixed-port design
would make `fghjd` simply fail to start on exactly the machines it's meant
to run on. Binding an OS-assigned port sidesteps the collision entirely, at
the cost of needing a discovery mechanism for whoever configures the OS
resolver to point at it — see below.

## Wiring into the OS resolver

`dns::install_os_resolver_config` (macOS: `install_macos_resolver`) writes
`/etc/resolver/fghj.internal`, a config file macOS's resolver subsystem
reads to route any query under that specific domain to a given
nameserver/port — the "zero-overhead" native integration `SPEC.md` calls
for, requiring no changes to `/etc/hosts` or the system-wide DNS
configuration. Because the DNS server's port is only known after `dns::bind`
actually runs, `run_control_api` calls `install_os_resolver_config` with
that concrete port immediately afterward, every `fghjd` startup — cheap and
idempotent (see `dns.rs`'s own test,
`install_macos_resolver_is_idempotent_and_writes_expected_content`), so
there's no harm in re-writing it even when nothing changed.

Linux (`systemd-resolved`) and Windows (NRPT) integration are described in
`SPEC.md` §5 but not implemented — see `PROGRESS.md`'s "Known gaps".

## Status

Implemented: `src/dns.rs` (wire-format parse/build, `bind`, `serve`,
`in_zone`, macOS resolver-file install). macOS-only for OS integration;
Linux/Windows print a manual-setup message instead of configuring anything.
