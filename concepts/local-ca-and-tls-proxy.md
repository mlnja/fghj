# Local CA and the TLS-terminating reverse proxy

## The problem

`fghj` wants every service reachable at a real HTTPS URL
(`https://cart.myworkspace.fghj.internal`) that behaves like production —
no `--insecure`, no browser warning, no plain HTTP. That means something on
the machine has to (a) own a certificate authority the OS/browser will
trust, and (b) terminate TLS for an unbounded, dynamically-changing set of
hostnames and forward the plaintext to whatever container is actually
running behind each one. Both jobs belong to `fghjd`, the root-owned
superdaemon (`src/web/ca.rs` + `src/web/proxy.rs`).

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

## Two CAs: the root signs one certificate, and it isn't a leaf

The root above signs exactly one thing: a subordinate **signing CA**
(`ca::ensure_signing_ca`, persisted as `ca/signing-{cert,key}.pem`), which
is what actually issues every leaf. The split is not PKI tidiness — it is
about where the private keys end up.

- The **root** key stays `0600` in `ca_dir()` and is never copied anywhere.
  It is the key the OS trust store vouches for, and it is
  `IsCa::Ca(Unconstrained)`, so a leak of it is a leak of trust for every
  hostname on the internet.
- The **signing** key cannot stay that private. `route_table`'s
  `refresh_sidecar_ca_copy` has to hand it to every run's sidecar container
  as a world-readable `0644` file, because Docker's macOS bind-mount bridge
  performs its permission check host-side, as the logged-in user, before
  the request reaches the container's UID namespace — a `0600` root-owned
  file is simply unreadable through it (the same constraint
  `refresh_sidecar_ca_copy`'s own doc comment records).

So one of the two keys is, unavoidably, readable by any local process and
present inside containers. Before the split that was the root, which is the
realistic leak path here — not "an attacker already has root". X.509
`nameConstraints` on the signing CA is what decides what that exposure
costs: it went from "every site on the internet" to "fghj's own zone and the
reserved alias TLDs".

`signing_name_constraints` permits exactly `permitted_dns_suffixes()` —
`dns::ZONE` plus every entry of `dns::RESERVED_ALIAS_TLDS`, read from those
same two constants so the constraint set cannot drift from what
`dns::cert_eligible` is willing to mint for. A `permittedSubtrees` of only
dNSName entries leaves every *other* name type wholly unconstrained, so
`0.0.0.0/0` and `::/0` are excluded explicitly; nothing fghj issues ever
carries an IP SAN. `BasicConstraints::Constrained(0)` caps it at signing
leaves and nothing below them.

### The migration has to scrub, not just stop

Splitting the CAs stops the root key *being copied* anywhere. It does
nothing about the copy already on disk: every install predating the split
has the root's private key at `sidecar-ca/ca-key.pem`, mode `0644`, because
that is what `refresh_sidecar_ca_copy` used to write there. Measured on this
machine before the upgrade — `-rw-r--r-- root wheel`, readable with no
`sudo`, and its public key's SHA-256 matching `ca/ca-cert.pem`'s exactly.

Nothing would have overwritten it until the next time a run happened to
start a sidecar, which on a machine where no run ever starts is never. So
`run_control_api` calls `refresh_sidecar_ca_copy` once at startup, right
after `ensure_signing_ca`: the function already writes exactly the bytes
that should be there, so running it up front *is* the migration. Best-effort
— a cleanup that fails should not stop the daemon, and `ensure_sidecar`
calls the same function and will report a real error if the copy is actually
needed.

Scrubbing is not the same as undisclosing, and that part is not fghj's call
to make. A key that spent weeks world-readable, and inside every sidecar
container, should be treated as disclosed: whoever holds a copy can mint a
cert for *any* hostname that this machine's trust store will accept, because
the root is `Unconstrained` by design. Regenerating it costs exactly one
Keychain Access prompt (delete `ca/ca-cert.pem`, `ca/ca-key.pem` and the
`signing-*` files, untrust the old cert, restart), and nothing outside this
machine depends on it, so the cost of rotating is about as low as a root
rotation ever gets.

### Why the constraints sit on a subordinate and not on the root

Because constraints on a trusted root are permanent in practice. Trust
settings attach to the root's bytes, so narrowing the root would mean that
ever supporting a real custom domain over HTTPS requires a *new* root — and
a fresh Keychain Access approval on every machine, which is precisely the
friction `ensure_ca`'s durability exists to avoid. With the constraints a
level down, widening the permitted set is minting a fresh signing CA under
the root every client already trusts: no re-approval, nothing to re-trust.

Adopting this needed no re-approval either, for the same reason in reverse
— the existing root was already `Unconstrained`, so it could sign a sub-CA
the day the code landed.

`ca/signing-generation` records *which* constraint set the persisted signing
CA was built with, derived from the set itself (`signing_generation()`)
rather than hand-maintained beside it. Editing `dns::RESERVED_ALIAS_TLDS`
therefore regenerates every install's signing CA on the next `fghjd` start,
with no constant to remember to bump. The marker is written last, so a crash
mid-way leaves it stale — which regenerates, the safe direction.

The sidecar needed no changes at all: `refresh_sidecar_ca_copy` writes the
signing material under the filenames the sidecar already loads
(`ca-cert.pem`/`ca-key.pem` in its own directory), so `fghj-sidecar.rs`
never learns there are two tiers.

## Issuing leaf certs on the fly

`ca::DynamicCertResolver` implements `rustls::server::ResolvesServerCert`:
for every incoming TLS handshake, it reads the SNI hostname the client
asked for, checks it's in-zone (`dns::in_zone` — reused here so "is this
name ours" has exactly one definition, shared with the DNS server), and
either returns a cached leaf cert for that exact name or mints a fresh one
signed by the **signing** CA and caches it. This is what makes an
arbitrarily deep `*.fghj.internal` name always "just work" over HTTPS
without any pre-generation step.

What it serves is a two-certificate chain — leaf plus the signing CA.
Clients trust the root, not the intermediate, so a leaf served alone would
fail with "unable to get local issuer certificate". Sending it stays correct
in the tests that hand the resolver a self-signed root instead: a chain may
include its own trust anchor, and verifiers ignore the extra cert.

That the constraints are *present* is a weaker claim than their being
*enforced*, so `the_signing_cas_constraints_are_enforced_by_a_real_verifier`
runs the served chain through the same webpki path a rustls client uses,
trusting only the root: `cart.fghj.internal` verifies, and a leaf minted for
`login.microsoftonline.com` — signed perfectly validly, via `issue` directly
so `cert_eligible` doesn't refuse it first — is rejected.

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
kept separate from `daemon::WorkspaceRegistry`/Docker so `web::proxy`'s own
test suite can exercise real TLS handshakes and byte-for-byte relaying
against a plain in-memory map, instead of needing live containers to test
routing logic at all (see `routed_in_zone_sni_proxies_to_its_registered_backend`
in `web::proxy`'s tests).

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

## Nothing here needs to be discovered

Three listeners come up together, and it's worth being precise about which
of them anyone has to *find*:

- **The proxy** (this subsystem) binds 80 and 443. Fixed, well-known, and
  the entire point — a browser has to be able to guess it.
- **The DNS server** binds an OS-assigned port (`dns::bind`), because
  nothing but the OS resolver ever dials it and the resolver is told where
  to look: `dns::install_os_resolver_config` writes the port it actually
  got into `/etc/resolver/fghj.internal`. This is the one case that really
  is "bind whatever the OS hands out, then publish it."
- **The control API** binds an OS-assigned TCP loopback port *and* a Unix
  socket at the fixed path `/var/run/fghjd.sock` (`daemon::socket_path`).
  The ephemeral TCP port is never published anywhere and never dialed by
  the CLI — only the in-process proxy relays to it, and it already holds
  the address it bound. The CLI uses the socket, whose path is a constant,
  so there is no discovery step at all: "can I connect to that path" is
  also the CLI's whole definition of "is `fghjd` up". See
  [[control-api-and-cli]].

What all three do share is the `/var/run` vs. `/var/lib/fghjd` split:
anything tied to *this* `fghjd` lifetime (the socket, the resolver config)
lives under `/var/run` and is expected to vanish on reboot, while anything
that must outlive a restart (the CA, the workspace index, the
active/idle flag) lives under `/var/lib/fghjd`.

## Two copies of the trust files, one of them mountable

`ca::refresh_trust_files` writes `cert.pem` (the CA cert alone) and
`bundle.pem` (that cert appended to this host's real root store, via
`rustls-native-certs`) as `0644`, key-free files. They are written into two
directories, and the duplication is load-bearing:

- `daemon::ca_dir()` — `/var/lib/fghjd/ca`. The path earlier versions
  documented for a hand-written `volumes:` entry. Nothing in fghj reads these
  two copies; they stay because a documented host path that stops existing
  does not fail a bind mount, it makes Docker invent an empty directory.
  This directory also holds both CAs' private keys, `0600`.
- `daemon::certs_dir()` — `/var/lib/fghjd/certs`. Bind-mounted `:ro` at
  `daemon::CERTS_MOUNT` (`/etc/fghj/certs`) into **every** container, by
  `node_spec`, ahead of the author's own volumes.

The second directory exists because the first one cannot be mounted. `:ro`
stops a container writing to a mount, not reading from it, and a container
running as root reads a root-owned `0600` file through a bind mount without
complaint — so neither `ca-key.pem` nor `signing-key.pem` may be in the
directory every container can see. The alternative shapes were worse: mounting the two files individually is
two binds instead of one and bind-mounting a *file* is the case
`persistence::fghjd_root`'s symlink resolution exists to work around, and a
symlink farm reintroduces that same hazard.

### Whose trust store `bundle.pem` actually mirrors

`bundle.pem` is only a safe drop-in replacement for a container's system
trust file if it really carries the roots this host trusts, so this got
measured rather than assumed. It is worth writing down because the obvious
conclusion was wrong.

macOS trust settings live in three domains — User, Admin, System — and
`rustls-native-certs` reads all three, but "User" means *the calling
process's* user. `fghjd` is root, so it reads root's own empty user domain
and, the reasoning went, silently drops every root the actual human trusted
in their login keychain: OrbStack's development CA lands there, as does a
corporate TLS-inspecting proxy's often enough. The fix looked obvious — re-
exec `fghjd` as the console user (uid/gid off `/dev/console`, `HOME` from
`getpwuid`) behind a `--print-native-roots` flag, and parse the PEM back.
That was built, and then measured against the root-written `bundle.pem`
already on disk:

| Enumerated as | Roots returned | Notably present |
|---|---|---|
| root (in-process) | 163 | `mitmproxy`, `aikido-l4-mitm-ca.localhost`, `Aikido Endpoint Protection Root CA`, `mkcert` |
| console user (re-exec) | 131 | `Aikido Endpoint Protection Root CA`, `mkcert` |

Root sees **more**, not less. Dropping privileges lost 32 roots including
two locally-trusted MITM proxy CAs — precisely the certificates whose
absence breaks a container's ordinary outbound HTTPS, which is the failure
the re-exec was built to prevent. And it did not even buy the thing it was
for: OrbStack's CA, which `security dump-trust-settings` confirms is in the
User domain with `SSL: kSecTrustSettingsResultTrustRoot`, is absent from
*both* enumerations — verified by SHA-256 fingerprint, with
`load_native_certs` reporting no per-domain errors at all. On macOS 26
`SecTrustSettingsCopyCertificates` simply does not surface it, whoever asks.

So the whole mechanism was removed. `native_root_certs` is a one-line call
again, and its doc comment records the measurement so the same idea doesn't
get rebuilt.

The user-domain gap is real, just not fghj's to close by re-execing: a CA
that exists only in someone's login keychain never reaches `bundle.pem`. The
fix is to trust it in the admin or System domain — where everything that
prompts for a password already puts it — or to concatenate it into a bundle
of one's own.

`merge_bundle` does skip fghj's own CA when the host store already lists it
— once `install_macos_trust` has run, `load_native_certs` returns it as a
trusted root like any other, and it was landing in `bundle.pem` twice
(measured: 2 subject lines out of 163). Harmless to a verifier, but the file
was lying about how many roots it carried, which is the kind of discrepancy
that sends someone debugging the wrong thing.

### Why fghj mounts but does not activate

Trusting an extra CA on Unix is almost always *replace*, not *add*:
`SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` and overwriting
`/etc/ssl/certs/ca-certificates.crt` all substitute the whole store, and
`NODE_EXTRA_CA_CERTS` is the only common additive form. Setting one of the
replacing kind on every container by default would silently exchange each
image's view of the public internet for fghj's, and the failure would surface
somewhere unrelated — a real OAuth provider, a real S3 — long after the
change. `bundle.pem` is what makes the replacing form safe *when asked for*,
which is the whole reason it is generated rather than leaving authors to
concatenate it themselves.

The cost of mounting into every container: `spec::spec_hash` hashes `binds`,
so adding this made every node in every workspace read as drifted exactly
once, and get recreated on the first run after the upgrade.

## Status

Implemented: `src/web/ca.rs` (root CA generation/persistence/trust install,
the name-constrained signing CA, dynamic per-SNI leaf issuance, the two
key-free trust files and their de-duplicated bundle), the
`/etc/fghj/certs` bind on every container (`daemon::certs_dir`,
`runs::node_spec`), `src/web/proxy.rs` (HTTP redirect, TLS
termination,
`RouteResolver`, apex vs. per-service dispatch, fancy-404 for unknown in-zone
names). `daemon::WorkspaceRegistry` implements `RouteResolver` over real run
state. Not implemented: any non-macOS trust-store install path (Linux would
need `update-ca-certificates` or equivalent; Windows, the platform CA
store) — see `PROGRESS.md`'s "Known gaps". Real per-service routing has not
been manually re-verified end-to-end since the flat-workspace/CUE-schema
refactor changed the on-disk fixtures' expected shape — also in
`PROGRESS.md`.
