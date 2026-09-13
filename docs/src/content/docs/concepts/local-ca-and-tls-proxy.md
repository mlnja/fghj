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
for a named port on a review run). A wildcard-per-depth-level scheme would
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

## Issuing leaf certs on the fly

For every incoming TLS handshake, `fghjd` reads the SNI hostname the
client asked for, checks it's in `fghj`'s zone, and either returns a
cached leaf cert for that exact name or mints a fresh one signed by the
local CA and caches it. This is what makes an arbitrarily deep
`*.fghj.internal` name always "just work" over HTTPS without any
pre-generation step.

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

Both the root CA and every leaf cert are built with [rcgen](https://docs.rs/rcgen), which
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

**Every leaf cert** (`ca::DynamicCertResolver::issue`) is signed by that CA
via `Issuer::from_ca_cert_der`, and is built with:

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
currently declared by a running node to `127.0.0.1` — every other name
under the same suffix is left alone. That block is kept in sync as
containers start and stop, and cleared entirely whenever `fghjd` goes
idle — via `fghj daemon stop` or the process shutting down outright.

Routing a hostname to a backend is decoupled from Docker behind a small
one-method interface, so the proxy's own test suite can exercise real TLS
handshakes and relaying against a plain in-memory map instead of needing
live containers just to test routing logic. In production, that interface
is backed by every wired workspace's active runs: it scans for a running
container whose registered routes claim the requested hostname, and
returns the host port Docker actually published that container's port on.
Only running containers are considered, so a stopped container's stale
route can't hand back a dead port.

Where those routes come from, and how they're derived and persisted, is
covered in [Run lifecycle & registry](/concepts/run-lifecycle-and-registry/);
how a route's domain itself is derived is covered in
[Node identity & domains](/concepts/node-identity-and-domains/).

## Limitations

Non-macOS trust-store installation isn't implemented yet — Linux would
need `update-ca-certificates` or equivalent, Windows the platform CA
store.
