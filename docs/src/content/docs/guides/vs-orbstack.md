---
title: fghj and OrbStack
description: Where the two overlap on domains and HTTPS, where OrbStack is the simpler answer, and the five things fghj's HTTP layer does that OrbStack's does not.
---

[OrbStack](https://orbstack.dev/) is a Docker Desktop replacement for macOS,
and it ships a feature set that overlaps fghj's squarely: every container
gets a domain (`service.project.orb.local`), there's a reverse proxy so you
don't type a port, certificates are minted per hostname by a CA it installs
into your keychain, and its root CA is injected into containers so TLS
verifies from the inside too.

If you have read [Local CA & TLS proxy](/concepts/local-ca-and-tls-proxy/)
and [Split DNS](/concepts/split-dns/), that list will sound familiar. The
overlap is real and this page does not pretend otherwise.

What the two do differently is the *shape* of what gets a name. OrbStack
names containers. fghj names **nodes in a resolved graph** — and that
difference turns into five concrete capabilities below.

## Where OrbStack is the better answer

Be clear about this first, because it's a large share of real cases.

If what you want is **one HTTP port, on one container, in one Compose
project, reached from the host**, OrbStack does it with less machinery than
fghj and you should use OrbStack. It owns the VM and the network stack, so
it needs no loopback alias, no `pf` rules, no `/etc/resolver` files and no
root daemon. It even detects your listening port by probing it, so you don't
declare one.

fghj needs all of that machinery because it runs your containers on *your*
Docker daemon rather than inside a VM it controls. That's a genuine cost:
most of [`fghj doctor`](/cli/doctor/) exists to diagnose a host integration
OrbStack doesn't have to build. fghj is also not a container runtime at all
— it has nothing to say about VM performance, file-sharing speed or
Kubernetes, and OrbStack is a reasonable thing to run fghj's containers on.

## 1. Non-HTTP services, by name, on their real port

OrbStack's domains route HTTP. Connecting to a database by domain —
`psql -h postgres.orb.local` — resolves and pings but fails to establish a
TCP connection, despite the documentation saying that non-HTTP services are
reachable by `host:port`. The fallback is the container's IP, which changes
when Compose recreates it, or a published host port, which is the
port-juggling the domain was supposed to remove.

fghj's [raw zone](/guides/networking-http-vs-raw/) is built for exactly
this. Each raw node gets its **own virtual IP** on a loopback alias, DNAT'd
to the port its author declared, so the canonical port is the port:

```bash
psql -h db.web.storefront.shop.fghj.raw.internal -p 5432
psql -h db.api.billing.shop.fghj.raw.internal    -p 5432
```

Two Postgres instances, both on 5432, no mapping table. The address is
derived from graph identity, so it survives a container being recreated.

## 2. More than one named port per service

A container that serves two HTTP things — an app port and a Vite dev server,
or an API and an admin console — can have only one OrbStack domain.
`dev.orbstack.http-port` is singular, and there is no syntax for binding a
second domain to a second port. The second port falls back to
`https://name.orb.local:5173`, which produces a certificate error.

In fghj, ports are a map and each entry can carry its own name:

```yaml
services:
  storefront:
    ports:
      "3000":
        primary: true
      "5173":
        name: vite
      "9229":
        name: admin
```

That's `storefront.shop.fghj.internal`, `vite.storefront.shop.fghj.internal`
and `admin.storefront.shop.fghj.internal` — three hostnames, three
certificates minted from the same local CA, one container. See
[`ports`](/reference/fghj-yaml/) for `name` and `wildcard`.

## 3. Cross-repo names that work from inside a container

This is the one that matters most for fghj's actual job.

Under OrbStack, each repo is its own Compose project. A container in project
A cannot reach `svc.projectB.orb.local` — it resolves from the host and fails
from inside a container, and OrbStack has **declined this as out of scope**
rather than filed it as a defect. So the multi-repo case is precisely the
unsupported one.

fghj puts a [sidecar](/concepts/sidecar/) inside each run's network that
serves DNS and terminates TLS for every node in that run, whatever repo it
came from. That's why a storefront container can call
`https://api.catalog.shop.fghj.internal` and get a valid certificate, with
the *same* address the host uses — see
[tutorial chapter 4](/tutorial/04-a-second-repo/#why-https-works-from-inside-the-container-too).

## 4. A TLD that survives a hostile resolver

`.local` is reserved for Multicast DNS by RFC 6762, so any network is
entitled to answer for it. When an ISP or corporate DNS server does,
`orb.local` resolution **hangs** — the system resolver wins. That isn't
straightforwardly fixable; it follows from the TLD.

fghj uses `.internal`, which ICANN designated for private use, and routes it
with a per-zone `/etc/resolver/fghj.internal` file. macOS consults that file
*by zone* and never sends the query to the default resolver, so an upstream
server claiming the zone never sees it.

One honest caveat on our side: fghj accepts author-declared
[`additional_hosts`](/reference/fghj-yaml/) under `.local` as well as
`.test`, `.internal` and `.localhost`. A **single** alias is written into
`/etc/hosts`, which beats DNS outright and is unaffected by any of this. A
`wildcard: true` alias needs a resolver file, so a wildcard under `.local`
inherits the same exposure. Prefer `.test` for wildcard aliases.

## 5. Your own TLS, and your own certificates

OrbStack's proxy expects to be the thing terminating TLS. A container that
generates a self-signed certificate at startup and serves HTTPS itself wins
over OrbStack's certificate, so the domain presents the untrusted one with
no documented override. There's also no way to get a leaf certificate *from*
its CA for your own server to present, which rules out testing your app's own
TLS path or mTLS. And its port detection probes your backend with a TLS
ClientHello, which makes some servers log spurious "connection that looks
like TLS received on a clear channel" warnings.

fghj declares the port rather than probing it, and
[its CA](/concepts/local-ca-and-tls-proxy/) is on disk under
`/var/lib/fghjd/ca`, so a certificate for your own server is something you
can mint.

## Summary

| | OrbStack | fghj |
|---|---|---|
| HTTP by name, host → container | Yes, zero config | Yes, declared port |
| Automatic per-hostname certs | Yes | Yes |
| CA trusted by the system | Yes | Yes |
| CA available inside containers | Yes, injected into the image | Yes, via the sidecar |
| Port auto-detection | Yes | No — `primary: true` |
| Non-HTTP by name, canonical port | No | Yes, the raw zone |
| Several named ports per service | No | Yes, `ports[].name` |
| Cross-project names from inside a container | No, declined | Yes, the sidecar |
| TLD safe from upstream DNS | No, `.local` | Yes, `.internal` |
| Serve your own TLS behind the name | No | Yes |
| Container runtime | Yes, that's the product | No |
| Multi-repo dependency graph | No | That's the product |
| Licensing | Free for personal use, paid for commercial | MIT |

## Reading this page later

**Every "no" above describes OrbStack as of October 2026**, established from
its documentation and its public issue tracker rather than from a local
install. It is actively developed, so treat each one as a thing to re-check
rather than as a fixed property — and if you find one has been fixed, this
page is wrong and worth a pull request.

Deliberately no issue links: a link to a report that has since been fixed
argues the opposite of what it was cited for, and a page of them rots faster
than the prose around it.

Two of the gaps look structural rather than scheduled. `.local` follows from
the TLD, not from a bug. And cross-project container-to-container naming was
declined as out of scope, which is a decision rather than a backlog item.

What won't change by OrbStack shipping fixes is the shape difference.
OrbStack names containers you have already defined. fghj names nodes in a
graph it resolved from per-repo config across many repos, which is where
items 2 and 3 come from and is what the rest of these docs are about.
