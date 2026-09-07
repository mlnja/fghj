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
