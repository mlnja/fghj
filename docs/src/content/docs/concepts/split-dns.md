---
title: Split DNS
description: A minimal, hand-rolled authoritative DNS server for *.fghj.internal, wired into the OS resolver.
---

## The problem

For `https://cart.myworkspace.fghj.internal` to work in a browser,
something has to answer that DNS query with `127.0.0.1` — your normal
resolver has never heard of `fghj.internal` and would just return
NXDOMAIN. `fghj` needs its own authoritative answer for exactly one zone,
without touching resolution for anything else on the machine.

## A hand-rolled server, on purpose

`fghjd` implements the DNS wire format directly rather than pulling in a
general-purpose DNS server library. The zone is fixed (`fghj.internal`),
the answer is always the same (`127.0.0.1`, with a short 5-second TTL so a
container restart's new route is picked up quickly instead of being
cached stale on the client), and every in-zone query gets that answer
while everything out-of-zone gets NXDOMAIN. The "is this name ours?" check
is a single shared implementation, reused by the TLS proxy's certificate
resolver (see [Local CA & TLS proxy](/concepts/local-ca-and-tls-proxy/)) so
both subsystems can never disagree about what's in-zone.

## Why an ephemeral port

The DNS server binds whatever port the OS hands out, rather than a fixed
port. `5353` — mDNS's well-known port — is routinely already bound by the
system's own mDNS responder or by Chrome on a real dev machine; a
fixed-port design would make `fghjd` fail to start on exactly the
machines it's meant to run on. Binding an OS-assigned port sidesteps the
collision entirely, at the cost of needing a discovery mechanism for
whoever configures the OS resolver to point at it.

## Wiring into the OS resolver

On macOS, `fghjd` writes `/etc/resolver/fghj.internal` — a config file the
system's resolver subsystem reads to route any query under that specific
domain to a given nameserver/port, with no changes needed to `/etc/hosts`
or the system-wide DNS configuration. Because the DNS server's port is
only known after it actually binds, `fghjd` writes this file with the
concrete port on every startup — cheap and idempotent, so there's no harm
in re-writing it even when nothing changed.

## Limitations

Linux (`systemd-resolved`) and Windows (NRPT) integration aren't
implemented yet — `fghjd` currently prints a manual-setup message on those
platforms instead of configuring anything.
