# Local CA and the TLS-terminating reverse proxy

## The problem

`fghj` wants every service reachable at a real HTTPS URL
(`https://cart.myworkspace.fghj.internal`) that behaves like production —
no `--insecure`, no browser warning, no plain HTTP. That means something on
the machine has to (a) own a certificate authority the OS/browser will
trust, and (b) terminate TLS for an unbounded, dynamically-changing set of
hostnames and forward the plaintext to whatever container is actually
running behind each one. Both jobs belong to `fghjd`, the root-owned
superdaemon (`src/ca.rs` + `src/proxy.rs`).

## Why one wildcard cert doesn't work

The obvious shortcut — issue a single cert for `*.fghj.internal` once, reuse
it everywhere — doesn't work here. X.509 wildcards only match **one**
leftmost label (RFC 6125): `*.fghj.internal` covers `cart.fghj.internal` but
not `cart.myworkspace.fghj.internal`, and fghj's names are arbitrarily deep
(`admin.cart.default.myworkspace.fghj.internal` for a named port on a review
run). A wildcard-per-depth-level scheme would need to be regenerated every
time the naming scheme grows a level, and still wouldn't handle depth
generically.

## The local CA

`ca::ensure_ca` generates a self-signed root CA exactly once and persists it
at `/var/lib/fghjd/ca/{ca-cert,ca-key}.pem` — **not** `/var/run`, because
`/var/run` is commonly tmpfs and wiped on reboot; a CA that didn't survive a
reboot would force the user to re-approve a brand-new one in Keychain Access
on every restart, defeating the entire point of trusting it once.

Installing that CA into the OS trust store (`ca::install_macos_trust`, via
`security add-trusted-cert` against the System keychain) is kept as a
**separate step** from generating it, so CA generation itself stays a pure,
easily-testable filesystem operation with no side effects on the host's
trust configuration.

`install_macos_trust` is safe to call on **every** `fghjd` start — it first
runs `security verify-cert` (`ca::is_trusted_on_macos`), a read-only trust
*evaluation* that never triggers a prompt, and only falls through to the
actual trust-store write when the cert genuinely isn't trusted yet. This
matters because modifying System keychain trust settings on macOS always
triggers an interactive Authorization Services password prompt — running as
root does **not** bypass it (root only bypasses filesystem permission
checks, a completely different gate) — so without the pre-check, a
crash-restart loop would re-prompt for a password on every single restart.
The check-first design doubles as a self-healing path: if trust is ever
missing for any reason (first-ever run, or a user manually revoking it via
Keychain Access while the persisted CA files remain on disk), the very next
`fghjd` start notices and re-installs it — prompting exactly once, only
when trust is actually absent.

## Issuing leaf certs on the fly

`ca::DynamicCertResolver` implements `rustls::server::ResolvesServerCert`:
for every incoming TLS handshake, it reads the SNI hostname the client
asked for, checks it's in-zone (`dns::in_zone` — reused here so "is this
name ours" has exactly one definition, shared with the DNS server), and
either returns a cached leaf cert for that exact name or mints a fresh one
signed by the local CA and caches it. This is what makes an arbitrarily deep
`*.fghj.internal` name always "just work" over HTTPS without any
pre-generation step.

TLS itself runs over `tokio_rustls` using the pure-Rust `ring` crypto
backend — deliberately **not** `aws-lc-rs`, which needs `cmake` at build
time and would make `fghj` a much less trivial `cargo build`/`curl | bash`
target.

## The reverse proxy

`fghjd` occupies ports 80 and 443, localhost-only
(`proxy::bind_http`/`bind_https`):

- **Port 80** does one thing: 301-redirect everything to the same path on
  `https://`. There is no plaintext serving.
- **Port 443** TLS-terminates via the resolver above, then dispatches the
  decrypted request based on the SNI name it was negotiated for:
  - The zone apex (`fghj.internal` itself) relays to the control API's
    port — this is what makes `https://fghj.internal` (no subdomain) serve
    the daemon's own HTTP API and the embedded UI.
  - Any other in-zone name is looked up via `proxy::RouteResolver` and, if
    found, relayed to the real backend container. An unrecognized in-zone
    name gets a "fancy 404" instead of a raw connection failure — the
    daemon is definitely listening for that zone, it just doesn't know that
    specific host.
  - Anything not in the zone at all: TLS handshake simply isn't attempted
    (this is also `DynamicCertResolver`'s rejection path — it has no cert
    to offer for a name it doesn't recognize as fghj's).

## `RouteResolver`: routing decoupled from Docker

```rust
pub trait RouteResolver {
    fn resolve(&self, host: &str) -> Option<u16>;
}
```

This one-method trait is the entire interface `proxy::serve_https` needs to
turn a hostname into a `127.0.0.1:<port>` to relay to. It's deliberately
kept separate from `daemon::WorkspaceRegistry`/Docker so `proxy.rs`'s own
test suite can exercise real TLS handshakes and byte-for-byte relaying
against a plain in-memory map, instead of needing live containers to test
routing logic at all (see `routed_in_zone_sni_proxies_to_its_registered_backend`
in `proxy.rs`'s tests).

`WorkspaceRegistry` implements `RouteResolver::resolve` via
`resolve_route`: it scans every wired workspace's active runs for a
`"running"` container whose `ContainerInfo.routes` claims the requested
hostname, and returns the host port Docker actually published that
container's port on. Only `"running"` containers are considered, so a
stopped-but-not-yet-reconciled container's stale route can't hand back a
dead port — though see [[run-lifecycle-and-registry]]'s reconciler section
and `PROGRESS.md`'s "Known gaps" for the one edge this doesn't quite close
(a *removed*, not just stopped, container's route can briefly outlive it
between reconcile ticks).

Where those routes actually come from is `runs::start_node` — see
[[node-identity-and-domains]] for how a route's domain is derived, and
[[run-lifecycle-and-registry]] for when `start_node` runs and how
`ContainerInfo.routes` gets persisted so routing survives a `fghjd` restart.

## Ephemeral-port + `/var/run` discovery, shared with DNS and the control API

The CA/proxy subsystem doesn't publish a discoverable port itself (443 is
fixed and well-known), but it's built and wired up alongside two other
pieces that do — the DNS server (`dns::bind` picks an OS-assigned port,
`dns::install_os_resolver_config` writes it into `/etc/resolver/fghj.internal`)
and the control API (binds an OS-assigned port, published at
`/var/run/fghjd.port` via `daemon::write_port`/`read_port`, mirroring the
existing `/var/run/fghjd.pid` convention). All three follow the same shape:
bind whatever the OS hands out, persist where to find it under `/var/run`
(ephemeral, tied to this `fghjd` lifetime — unlike the CA's durable
`/var/lib/fghjd/ca`), and have the unprivileged `fghj` CLI or the OS
resolver discover it from there instead of hardcoding a fixed port that
might already be taken by something else on a real dev machine.

## Status

Implemented: `src/ca.rs` (CA generation/persistence/trust install, dynamic
per-SNI leaf issuance), `src/proxy.rs` (HTTP redirect, TLS termination,
`RouteResolver`, apex vs. per-service dispatch, fancy-404 for unknown in-zone
names). `daemon::WorkspaceRegistry` implements `RouteResolver` over real run
state. Not implemented: any non-macOS trust-store install path (Linux would
need `update-ca-certificates` or equivalent; Windows, the platform CA
store) — see `PROGRESS.md`'s "Known gaps". Real per-service routing has not
been manually re-verified end-to-end since the flat-workspace/CUE-schema
refactor changed the on-disk fixtures' expected shape — also in
`PROGRESS.md`.
