# Split-DNS: a minimal authoritative server for `*.fghj.internal`

## The problem

For `https://cart.myworkspace.fghj.internal` to work in a browser, something
has to answer that `A` query with a local address — the OS's normal resolver
(talking to whatever DNS the network/VPN provides) has never heard of
`fghj.internal` and would just NXDOMAIN it. `fghj` needs its own
authoritative answer for exactly one zone, without touching resolution for
anything else on the machine.

## A hand-rolled server, on purpose

`src/dns/` implements the DNS wire format directly — `parse_query`/
`build_response` — rather than pulling in a general-purpose DNS server
crate. The zone is fixed (`ZONE = "fghj.internal"`), the answer is always
the same (`127.222.0.1`, TTL 5s — short, so a container restart's new
container name/route is picked up quickly rather than being cached stale on
the client), and every in-zone query gets that answer while everything
out-of-zone gets `NXDOMAIN`. `dns::in_zone` — checking a query name against
`ZONE_SUFFIX = ".fghj.internal"` (plus the bare apex) — is `pub(crate)` and
reused by `ca::DynamicCertResolver`, so "is this name ours" has exactly one
implementation shared between the two subsystems that both need to answer
it (see [[local-ca-and-tls-proxy]]).

## Why `127.222.0.1` and not `127.0.0.1`

`dns::ANSWER` is `web::proxy::PROXY_IP`, a loopback address of `fghjd`'s own
that it binds :80/:443 on. macOS has only `127.0.0.1` on `lo0`, so there it
is aliased first (`platform::Loopback::add_alias`, called from
`DaemonControl::activate`); Linux already routes all of `127/8` to `lo`. Two separate decisions are
baked into that constant.

**Why not `127.0.0.1`.** Binding the machine's usual loopback address at
:80/:443 makes fghj hostile to install. A developer already running a local
nginx or a `docker run -p 80:80` would either be unable to start `fghjd`, or
— worse, because it's silent — would find their own `curl http://127.0.0.1/`
answered by fghj instead of by their container, since BSD delivers to the
most specific bind. Taking a dedicated address leaves `127.0.0.1:80`,
`127.0.0.1:443` and `0.0.0.0:80` free, so installing fghj cannot disturb a
setup that was already working. This is a deliberate adoption property: the
tool has to be safe to *try*.

**Why `127/8`.** RFC 1122 forbids a host from emitting a loopback-destined
datagram onto the wire, and routers drop it — so no LAN, VPN or corporate
network can ever collide with an address in `127/8`. `10/8` is ordinary
RFC 1918 space that real sites do carve up, and a virtual IP there can
shadow a host the developer actually needs; the raw zone's pool,
`127.223.0.0/16`, is in loopback for the same reason (see
[[two-zones-and-raw-ports]]). Docker's embedded resolver (`127.0.0.11`) and
systemd-resolved (`127.0.0.53`) rely on the same guarantee.

**Why an alias rather than DNAT.** Redirecting `PROXY_IP:443` to an
unprivileged high port with a kernel NAT rule was considered, and would
additionally free ports 80/443 machine-wide. It was rejected because it puts
the user-facing path behind firewall rules, which any other program that
reloads the firewall can switch off. A plain socket cannot be turned off from
outside, and the raw zone listens on its own sockets for the same reason. The one
case an alias does not cover is a server binding `0.0.0.0:80` *without*
`SO_REUSEADDR`; that is rare (nginx, Apache, Caddy, Docker, Go and Node all
set it) and such a server already fails against any other holder of port 80.

Root is still required either way — `ifconfig` or `ip`, the CA, the OS
resolver config, `/etc/hosts`, and binding a privileged port on any
address.

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

`dns::install_os_resolver_config` hands the zones to the platform's
`DnsRouting`. On macOS that's `platform/macos/resolver_dir.rs`, which writes
`/etc/resolver/<zone>` files, the config macOS's resolver subsystem reads to route any query under that
specific domain to a given nameserver/port — the "zero-overhead" native
integration `SPEC.md` calls for, requiring no changes to `/etc/hosts` or the
system-wide DNS configuration. Because the DNS server's port is only known
after `dns::bind` actually runs, `run_control_api` calls
`install_os_resolver_config` with that concrete port immediately afterward,
every `fghjd` startup — cheap and idempotent (see
`resolver_dir.rs`'s `sync_resolver_is_idempotent_and_writes_expected_content`), so
there's no harm in re-writing it even when nothing changed.

It is a *sync*, not a single write, because the set of zones is not fixed.
Two of them are: `fghj.internal` and `fghj.raw.internal`
([[two-zones-and-raw-ports]] explains why there are two). The rest are
whatever `wildcard_hosts` suffixes the currently-running containers declare,
so they come and go — which is why `install_os_resolver_config` is called on
every reconcile tick and not only at startup, and why
`resolver_dir.rs`'s `sync_resolver` removes fghjd-authored files for zones that are no
longer live. It only ever touches files it recognizes as its own
(`is_fghjd_resolver_content`, which parses the port back out of fghjd's own
template); a hand-written `/etc/resolver` file for some unrelated domain is
left alone. `DnsRouting::clear` removes all of them on a clean
shutdown, so a stopped `fghjd` doesn't leave names pointed at a dead port.

On Linux the same routing goes through systemd-resolved
(`src/platform/linux/resolved.rs`). Resolved's global `DNS=` is always a
default route, so pointing it at fghjd would send it every lookup on the
machine; a per-link server only gets the domains the link claims. So
`fghjd` creates a dummy link `fghj0`, gives it `127.222.0.53` (resolved
ignores a link with no address wider than link scope), and sets its server
to `127.0.0.1:<port>` and its domains to `~fghj.internal ~fghj.raw.internal`,
with default-route, LLMNR, mDNS and DNSSEC off; DNSSEC because `.internal`
is provably absent from the signed root. The settings are runtime-only:
resolved keeps them across its own restarts, and deleting the link on stop
drops them. systemd 258's `dns-delegate.d` would do this without a link,
but Ubuntu 22.04 and 24.04 ship 249 and 255. Windows (NRPT) is described in
`SPEC.md` §5 but not implemented.

## Related

- [[two-zones-and-raw-ports]] — what the second zone is for, and what a
  query in each zone is actually answered with.
- [[in-network-sidecar]] — the other DNS server in the system: this one
  serves the host, the sidecar serves a run's own container network.
- [[local-ca-and-tls-proxy]] — what the address this server hands back is
  actually running.

## Status

Implemented: `src/dns/` (wire-format parse/build, `bind`, `serve`,
`in_zone`), with OS integration through `/etc/resolver` on macOS and
systemd-resolved on Linux. Other platforms print a manual-setup message
instead of configuring anything.
