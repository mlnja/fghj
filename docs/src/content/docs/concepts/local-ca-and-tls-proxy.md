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

## The reverse proxy

`fghjd` occupies ports 80 and 443, localhost-only:

- **Port 80** does one thing: redirects everything to the same path on
  `https://`. There is no plaintext serving.
- **Port 443** terminates TLS, then dispatches the decrypted request based
  on the SNI name it was negotiated for:
  - The zone apex (`fghj.internal` itself) relays to the control API —
    this is what makes `https://fghj.internal` serve the daemon's own API
    and the embedded UI.
  - Any other in-zone name is looked up against the currently running
    containers and, if found, relayed to the real backend. An
    unrecognized in-zone name gets a friendly 404 instead of a raw
    connection failure — the daemon is definitely listening for that
    zone, it just doesn't know that specific host yet.
  - Anything not in the zone at all: the TLS handshake simply isn't
    attempted — there's no cert to offer for a name `fghj` doesn't
    recognize.

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
