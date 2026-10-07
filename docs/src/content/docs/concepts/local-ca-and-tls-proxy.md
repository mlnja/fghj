---
title: Local CA & TLS proxy
description: How fghj issues trusted HTTPS certs for an arbitrarily deep, dynamically changing set of *.fghj.internal hostnames.
---

## The problem

`fghj` wants every service reachable at a real HTTPS URL
(`https://cart.myworkspace.fghj.internal`) that behaves like production —
no `--insecure`, no browser warning, no plain HTTP. That means something on
the machine has to own a certificate authority the OS/browser trusts, and
terminate TLS for an unbounded, dynamically changing set of hostnames,
forwarding the plaintext to whatever container is actually running behind
each one. Both jobs belong to `fghjd`.

## Why one wildcard cert doesn't work

The obvious shortcut — issue a single cert for `*.fghj.internal` once,
reuse it everywhere — doesn't work. X.509 wildcards only match one
leftmost label (RFC 6125): `*.fghj.internal` covers
`cart.fghj.internal` but not `cart.myworkspace.fghj.internal`, and fghj's
names are arbitrarily deep (`admin.cart.default.myworkspace.fghj.internal`
for a named port on a named run). A wildcard-per-depth-level scheme would
need regenerating every time the naming scheme grows a level, and still
wouldn't handle depth generically.

## The local CA

`fghjd` generates a self-signed root CA exactly once and persists it under
`/var/lib/fghjd/ca/` — not a tmpfs-backed location, because a CA that
didn't survive a reboot would force you to re-approve a brand-new one in
your OS's certificate trust UI on every restart, defeating the entire
point of trusting it once.

Installing that CA into the OS trust store is a separate step from
generating it, and it's safe to run on every `fghjd` start: it first
checks whether the cert is already trusted (a read-only check that never
triggers a prompt) and only falls through to actually writing trust when
it genuinely isn't trusted yet. This matters because modifying system
trust settings always triggers an interactive password prompt — running
as root doesn't bypass it. Without the pre-check, a crash-restart loop
would re-prompt for a password on every single restart. The same
check-first design doubles as a self-healing path: if trust is ever
missing (first-ever run, or you manually revoke it while the CA files
remain on disk), the next `fghjd` start notices and re-installs it,
prompting exactly once.

## Two CAs, because one key has to leave the daemon

The root CA above signs exactly one certificate, and it isn't a leaf. It
signs a subordinate **signing CA** (`/var/lib/fghjd/ca/signing-cert.pem`),
and that is what issues every leaf cert `fghjd` serves.

The reason is that one of the two private keys cannot be kept private. Each
run gets a [sidecar proxy](#reaching-the-proxy-from-inside-a-runs-own-network)
that terminates TLS inside the run's own docker network, so it needs a
signing key of its own — mounted in as a world-readable file, because
Docker's macOS file sharing checks permissions host-side, as the logged-in
user, before the request ever reaches the container's user namespace: a
root-only `0600` file is simply unreadable through it.

So the split is about which key that is:

| | Root CA | Signing CA |
|---|---|---|
| Trusted by your OS | yes | no — trusted transitively, through the root |
| Private key | `0600`, root-only, never leaves `/var/lib/fghjd/ca/` | copied world-readable into every run's sidecar |
| What it signs | the signing CA, and nothing else | every leaf cert |
| Names it may certify | anything | only the suffixes below |

The signing CA carries an X.509 `nameConstraints` extension permitting
`fghj.internal` and the four reserved alias TLDs (`.local`, `.test`,
`.internal`, `.localhost`) as DNS subtrees, excluding the whole IPv4 and
IPv6 address space, and capped (`basicConstraints` path length 0) at signing
leaf certificates rather than further CAs. A `permittedSubtrees` listing
only DNS names leaves every other name type unconstrained, which is why the
IP exclusions are there and not merely belt-and-braces. The permitted set is
built from the same constants the issuance check reads, so the two can't
drift into disagreeing about which names are eligible.

Concretely: if that world-readable signing key is read by anything on your
machine, or by anything inside a container, the certificates it can mint are
limited to fghj's own zone and the reserved special-use TLDs. It cannot
produce a certificate for `login.microsoftonline.com` that any verifier will
accept — and that's checked by running the real chain through the same
verification path a TLS client uses, not just by asserting the extension is
present. Without the split, the key in that position was the root's, and the
answer was "any hostname on the internet".

### Upgrading from a version before the split

Before the signing CA existed, the key copied into each sidecar was the
**root's**, and it was world-readable (`0644`) by necessity — the macOS
bind-mount bridge leaves no alternative. If you ran one of those versions,
that copy is still on disk at `/var/lib/fghjd/sidecar-ca/ca-key.pem`.

The first `fghjd` start after upgrading overwrites it with the
name-constrained signing key, so no run or other action is needed on your
part. But overwriting it is not the same as un-disclosing it. For however
long it was there, any process on the machine — and anything running inside
any fghj container — could read a private key whose certificate your OS
trusts for *every* hostname. If that matters to you, regenerate the root:

```sh
# 1. stop the daemon, so it isn't restarting mid-rotation
sudo brew services stop fghj     # or: sudo launchctl bootout system/sh.brew.fghj

# 2. untrust and delete the old root, and the signing CA under it
sudo security delete-certificate -c "fghj local CA" \
  /Library/Keychains/System.keychain
sudo rm /var/lib/fghjd/ca/ca-cert.pem /var/lib/fghjd/ca/ca-key.pem \
  /var/lib/fghjd/ca/signing-cert.pem /var/lib/fghjd/ca/signing-key.pem \
  /var/lib/fghjd/ca/signing-generation

# 3. mint the new root and trust it — from a terminal, see below
sudo fghjd            # Ctrl-C once it logs "control API listening on ..."

# 4. hand it back to the service manager
sudo brew services start fghj
```

:::caution[Step 3 has to come from a terminal]
Installing a root into the System trust store needs an Authorization
Services prompt, and **running as root does not bypass it** — that gate is
about user interaction, not file permissions. A launchd *system* daemon has
no session to show a prompt in, so `fghjd` started by `brew services` fails
with `SecTrustSettingsSetTrustSettings: The authorization was denied since
no user interaction was possible`, exits, and — with `KeepAlive` set — is
restarted into the same failure indefinitely. `fghj doctor` then reports
only that the daemon isn't running.

Running `sudo fghjd` once in your own terminal gives it a session to prompt
in. After that the trust check at the top of every start is a no-op, so the
service manager can take over again. If you'd rather not run the daemon in
the foreground at all, install the trust directly — it's the same command
`fghjd` runs:

```sh
sudo security add-trusted-cert -d -r trustRoot \
  -k /Library/Keychains/System.keychain /var/lib/fghjd/ca/ca-cert.pem
```
:::

Nothing outside this machine depends on the old root, so the whole rotation
costs one password prompt.

### Why the constraints are on the subordinate, not the root

Constraints on a trusted root are permanent in practice. Trust attaches to
the root's exact bytes, so narrowing the root would mean that ever
supporting HTTPS on a real custom domain requires a *new* root CA — and a
fresh trust-store approval on every machine, which is exactly the friction
the root's durability exists to avoid. One level down, widening the
permitted set is just minting a fresh signing CA under the root your machine
already trusts: no re-approval, nothing to re-trust.

A small marker file, `/var/lib/fghjd/ca/signing-generation`, records which
constraint set the persisted signing CA was built with, so changing the
eligible-suffix list regenerates it automatically on the next `fghjd` start.

## Issuing leaf certs on the fly

For every incoming TLS handshake, `fghjd` reads the SNI hostname the
client asked for, checks it's in `fghj`'s zone, and either returns a
cached leaf cert for that exact name or mints a fresh one signed by the
signing CA and caches it. This is what makes an arbitrarily deep
`*.fghj.internal` name always "just work" over HTTPS without any
pre-generation step. What gets served is a two-certificate chain — the leaf
plus the signing CA — since your machine trusts the root, not the
intermediate, and a leaf sent on its own would fail with "unable to get
local issuer certificate".

The same eligibility check also covers a service's declared
[additional hosts](/reference/fghj-yaml/#additional-hosts): a literal
alias under an IANA reserved special-use TLD (`.local`, `.test`,
`.internal`, `.localhost`) is treated the same as an in-zone name — but
only while some running container's routes actually claim it. This keeps
the CA's blast radius bounded: it never issues a cert just because a
hostname *looks* reserved, only when it's both reserved-shaped and
currently backed by a real route. A non-reserved alias (anything that
could be a real, internet-routable domain) never gets a certificate at
all — see below.

## What's actually in each certificate

All three certificates — the root CA, the signing CA and every leaf — are
built with [rcgen](https://docs.rs/rcgen), which
defaults to a minimal, RFC-legal-but-not-defensive certificate: no
`SubjectKeyIdentifier` (SKI), no `AuthorityKeyIdentifier` (AKI), and no
`basicConstraints`, unless the caller explicitly opts in. `fghjd` opts in
deliberately for both certs it mints, for different reasons.

**The root CA** (`ca::generate_ca`) is self-signed, so RFC 5280 doesn't
*require* an AKI on it (issuer and subject are the same key by
construction). It's created with:

- `is_ca = IsCa::Ca(BasicConstraints::Unconstrained)` — marks it as a CA
  with no path-length limit, and (as a side effect of rcgen only writing
  `SubjectKeyIdentifier`/`basicConstraints` when `is_ca` isn't left at its
  default `NoCa`) is what gives the root its own SKI. That SKI is what
  every leaf's AKI below points back to.
- `key_usages = [KeyCertSign, CrlSign]` — the two usages meaningful for a
  CA key: signing certificates and (were fghj ever to issue one) a CRL.

**The signing CA** (`ca::ensure_signing_ca`) is signed by that root, and
adds `nameConstraints` plus `BasicConstraints::Constrained(0)` to the same
`KeyCertSign`/`CrlSign` usages — see [above](#two-cas-because-one-key-has-to-leave-the-daemon)
for what those two do and why they're here rather than on the root. Being
non-self-signed, it carries an AKI pointing back at the root for the same
RFC 5280 reason every leaf does.

**Every leaf cert** (`ca::DynamicCertResolver::issue`) is signed by the
*signing* CA via `Issuer::from_ca_cert_der`, and is built with:

- `CertificateParams::new(vec![name])`, which populates the
  `SubjectAlternativeName` extension with `name` as a DNS SAN — the actual
  field TLS clients check against the SNI they asked for (the `CommonName`
  is set too, but is legacy/cosmetic by comparison).
- `use_authority_key_identifier_extension = true` — writes an AKI whose
  key identifier is derived from the CA's own SKI (via
  `Issuer::from_ca_cert_der`, which parses the issuer cert's existing SKI
  extension rather than recomputing one). This is the field that was
  missing before this was added, and it's not optional: RFC 5280 §4.2.1.1
  requires a non-self-signed certificate to carry an AKI. Some verifiers
  enforce that literally — a newer OpenSSL (and anything linked against
  it, e.g. a `pip`/`twine` upload from a Python built against it) will
  flatly reject a chain whose leaf has no AKI, even though `openssl
  s_client`, curl, and browsers are lenient about it and connect anyway.
  That leniency gap is exactly why this class of bug can ship unnoticed:
  the everyday manual check ("does it curl?") passes, and only a stricter
  client surfaces it.
- `is_ca = IsCa::ExplicitNoCa` — explicitly marks the leaf as *not* a CA
  (as opposed to just leaving `is_ca` at rcgen's default `NoCa`, which
  skips writing the extension at all). This is what turns on the leaf's
  own `SubjectKeyIdentifier` and an explicit `basicConstraints: CA:FALSE`
  — both technically optional for an end-entity cert per RFC 5280, but
  cheap to include and exactly what a "real" CA-issued server cert looks
  like.
- `key_usages = [DigitalSignature, KeyEncipherment]` and
  `extended_key_usages = [ServerAuth]` — the usages a TLS server
  certificate is actually expected to declare.

The round trip (leaf's AKI must equal the CA's SKI) is asserted directly in
`ca::tests::issued_leaf_cert_carries_aki_ski_and_basic_constraints`, alongside
the existing `issued_leaf_cert_chains_to_the_ca` test that checks the
cryptographic signature itself verifies against the CA's public key —
between the two, both "is this chain trustworthy" and "is this chain
*shaped* the way a strict verifier expects" are covered.

## The reverse proxy

`fghjd` occupies ports 80 and 443, localhost-only:

- **Port 80** redirects to the same path on `https://` for anything
  in-zone or under a reserved alias TLD. The one exception is a declared,
  currently-routed *non-reserved* additional host (e.g.
  `app.local.aikido.io`) — since that alias can never get a certificate,
  redirecting it to HTTPS would be a dead end, so it's relayed over plain
  HTTP instead. This is the only path that ever serves plaintext.
- **Port 443** terminates TLS, then dispatches the decrypted request based
  on the SNI name it was negotiated for:
  - The zone apex (`fghj.internal` itself) relays to the control API —
    this is what makes `https://fghj.internal` serve the daemon's own API
    and the embedded UI.
  - Any other in-zone name, or a routed reserved-alias additional host, is
    looked up against the currently running containers and, if found,
    relayed to the real backend. An unrecognized in-zone name gets a
    friendly 404 instead of a raw connection failure — the daemon is
    definitely listening for that zone, it just doesn't know that specific
    host yet.
  - Anything else: the TLS handshake simply isn't attempted — there's no
    cert to offer for a name `fghj` doesn't recognize or isn't allowed to
    certify.

## Resolving additional hosts

An in-zone `*.fghj.internal` name resolves through `fghjd`'s own DNS
server, covered in [Split DNS](/concepts/split-dns/). A literal
additional host like `aikido.local` is a real hostname that already means
something else on the network (or nothing at all) — delegating a whole
suffix like `.local` to `fghjd`'s DNS would hijack every other lookup
under it, including mDNS device discovery. Instead, `fghjd` manages a
marked block inside `/etc/hosts`, pinning only the exact hostnames
currently declared by a running node to `127.222.0.1` — every other name
under the same suffix is left alone. That block is kept in sync as
containers start and stop, and cleared entirely whenever `fghjd` goes
idle — via `fghj daemon stop` or the process shutting down outright.

Routing a hostname to a backend is decoupled from Docker behind a small
one-method interface, so the proxy's own test suite can exercise real TLS
handshakes and relaying against a plain in-memory map instead of needing
live containers just to test routing logic:

```rust
pub struct Backend {
    pub host: String,
    pub port: u16,
}

pub trait RouteResolver: Send + Sync {
    fn resolve(&self, host: &str) -> Option<Backend>;
}
```

Returning a full `Backend` (host *and* port), not just a port, is what lets
the same trait and the same `proxy::serve_https`/`serve_http_redirect`
logic back both the host-side proxy and the in-network sidecar proxy
described below — the two differ only in what they resolve a hostname to.

In production on the host, that interface is backed by every wired
workspace's active runs: it scans for a running container whose registered
routes claim the requested hostname, and returns `127.0.0.1` plus the host
port Docker actually published that container's port on. Only running
containers are considered, so a stopped container's stale route can't hand
back a dead port.

Where those routes come from, and how they're derived and persisted, is
covered in [Run lifecycle & registry](/concepts/run-lifecycle-and-registry/);
how a route's domain itself is derived is covered in
[Node identity & domains](/concepts/node-identity-and-domains/).

## Reaching the proxy from inside a run's own network

The host-side proxy above is bound to `127.222.0.1`, reachable from the host
but not from inside a run's own docker network. A service can need the
*same* HTTPS hostname to work both for its own internal calls and for the
URLs it hands out to external consumers (a presigned S3 URL is the
motivating case) — only the host-side proxy has a TLS listener behind that
name, and only the host can reach it, so plain in-network DNS resolving
straight to a sibling's IP would bypass the proxy (and TLS) entirely for
that case.

`fghjd` solves this with one dedicated sidecar container per run, attached
to that run's own docker network, running the exact same `RouteResolver`
+ `serve_https`/`serve_http_redirect` logic as the host-side proxy — a
separate `fghj-sidecar` binary, not a mode flag on `fghjd`, since it needs
a signing key mounted in and otherwise deserves the smallest possible
attack surface. That key is the name-constrained signing CA's, never the
root's — see [Two CAs](#two-cas-because-one-key-has-to-leave-the-daemon). It doesn't talk to `fghjd` over the network at
all: `fghjd` writes a small JSON route table to a bind-mounted file every
time a run's containers or routes change, and the sidecar polls that
file's mtime once a second and reloads it — no dependency on any
particular container runtime's network-event or filesystem-event
propagation, which is exactly the source of the platform-specific
workarounds this design replaces. Each route entry names the owning
sibling container's own `fghj.raw.internal` domain/alias as the connect
target, so Docker's own per-network DNS resolves it for the sidecar
exactly like it would for any other container — no separate IP
bookkeeping needed.

Reaching this path needs no per-consumer setup: the sidecar also runs this
run's DNS authority for the whole network (see [Split
DNS](/concepts/split-dns/)), and every node's container points its `--dns`
there first. Every `*.fghj.internal` name and active alias resolves to the
sidecar's own IP automatically, from inside the network, with the same
hostname a browser outside it would use — no `extra_hosts` entry, no
opt-in. One sidecar per run (never shared across runs or workspaces) keeps
the same network isolation every other part of a run's docker network
already has.

## Trust files for containers

A container that dials an in-zone name over HTTPS has to trust fghj's local
CA, and neither the host-side proxy nor the sidecar can inject that trust
into the container's TLS library for it. What `fghjd` can do — and does — is
make sure the material is always *present*, so the author's side of this is
one line of config rather than a mount, a path and a refresh story.

`ca::refresh_trust_files` writes two world-readable, key-free files on every
`fghjd` start:

- **`cert.pem`** — just fghj's CA cert, PEM-encoded, no key material.
- **`bundle.pem`** — that same cert merged with this host's own real root
  CA store (via [`rustls-native-certs`](https://docs.rs/rustls-native-certs)),
  so it is a *complete* trust store rather than a single cert: a drop-in
  replacement for a container's entire system trust file, with the real
  public CAs still trusted alongside fghj's.

They are written twice, into two directories, and the duplication is the
point:

| Directory | Who reads it |
|---|---|
| `/var/lib/fghjd/ca/` | Nothing, now — it also holds both CAs' certificates and their `0600` private keys. Kept because it is the path earlier versions documented, and a `volumes:` entry naming a host path that vanished would make Docker invent an empty directory rather than fail. |
| `/var/lib/fghjd/certs/` | **Every container.** `fghjd` bind-mounts this directory read-only at `/etc/fghj/certs` in every container it starts. |

The split exists for one reason: `:ro` prevents a container *writing* to a
mount, not *reading* from it, and a container running as root will read a
root-owned `0600` file through a bind mount quite happily. So the directory
that gets mounted into every container has to be one no private key was ever
in. Nothing about the files themselves differs between the two copies.

So from inside any container, both files are simply there:

```yaml
environment:
  # replaces the trust store, so it wants the full bundle
  SSL_CERT_FILE: /etc/fghj/certs/bundle.pem
```

```yaml
environment:
  # Node's is additive, so it wants the bare cert
  NODE_EXTRA_CA_CERTS: /etc/fghj/certs/cert.pem
```

`bundle.pem` carries the roots `fghjd` can see, which on macOS is the Admin
and System trust domains. Certificates trusted only in your **User** domain
— in your login keychain, without an admin prompt — are not in it. OrbStack's
development CA is the common example. If a container needs to trust one of
those, trust it at the admin/System level instead (which is where anything
that asked you for a password already put it), or concatenate it into a
bundle of your own.

fghj's own CA is de-duplicated out of the merge, since once it's installed in
your System keychain the enumeration returns it like any other root and it
would otherwise appear in the bundle twice.

**Mounting is where fghj stops, deliberately.** Almost every mechanism Unix
offers for trusting an extra CA *replaces* the trust store instead of
extending it — `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, overwriting
`/etc/ssl/certs/ca-certificates.crt`; Node's `NODE_EXTRA_CA_CERTS` is the
only common additive one. Setting any of the first kind automatically, on
every container, would swap each image's idea of the public internet for
fghj's, invisibly, until some unrelated outbound call failed verification.
That is not a change to make on an author's behalf. `bundle.pem` exists
precisely so that when an author *does* ask, the replacing form is safe.

`refresh_trust_files` is a separate step from generating/loading the CA, not
folded into it, because the sidecar's own startup loads the CA against a `:ro`
bind mount of the CA directory (see above) and would fail if that shared code
path tried to write back into it — only a caller that owns a writable copy of
the directory (`daemon::run_control_api`, on the host) calls it. Since fghj's
CA itself is essentially never regenerated in ordinary use (it's meant to
survive indefinitely, precisely so you never have to re-approve trust — see
[The local CA](#the-local-ca) above), these files don't need any periodic
reconciliation either; a one-time refresh at daemon startup keeps every copy
correct. See the [HTTP vs. raw
guide](/guides/networking-http-vs-raw/#trusting-fghjs-ca-inside-a-container)
for full worked examples — Java, a shell that appends rather than replaces,
and the presigned-URL case that motivated this.

## Limitations

Non-macOS trust-store installation isn't implemented yet — Linux would
need `update-ca-certificates` or equivalent, Windows the platform CA
store.

Nothing *activates* the trust files mounted into each container; they are
only made available. See the reasoning under [Trust files for
containers](#trust-files-for-containers) — the mechanisms that would do it
replace a container's whole trust store, which is not a thing to do
silently. The one case where this is felt as a real gap is an image with no
shell, no writable root filesystem and a TLS stack that ignores
`SSL_CERT_FILE`; there is no known common runtime in that intersection.

The mount is a container-time thing, not a build-time one: a `RUN` step in a
Dockerfile cannot read `/etc/fghj/certs`, so a build that needs to dial the
http zone needs `build.secrets` or the raw zone instead.

Making every node's DNS resolution depend on the sidecar being up is a
larger blast radius than before this split existed, when a node's own
container needed nothing beyond Docker's own embedded resolver. Mitigated,
not eliminated: every node's `--dns` list falls back to Docker's embedded
resolver (`127.0.0.11`) second, so it only ever kicks in if the sidecar is
genuinely unreachable (a timeout), not on every ordinary query.
