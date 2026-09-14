---
title: "Guide: choosing HTTP, HTTPS, or raw for a service call"
description: A practical, task-oriented walkthrough of fghj's two DNS zones — which macro to use, when a container needs CA trust, and how the same setup behaves inside vs. outside the run's network.
---

This page is the "how do I actually configure this" companion to a handful
of concept pages that each cover one piece of the underlying design: [Node
identity & domains](/concepts/node-identity-and-domains/), [Split
DNS](/concepts/split-dns/), [In-network TLS proxy
sidecar](/concepts/sidecar/), and [Local CA & TLS
proxy](/concepts/local-ca-and-tls-proxy/). Read those for the *why*; this
page is the *how*, with worked examples pulled from real bugs found while
wiring up a real multi-repo workspace.

## The one-paragraph mental model

Every node gets **two** domains, always differing only in suffix:

- **`{node}.{workspace}.fghj.raw.internal`** — raw/direct. Resolves via
  Docker's own per-network DNS straight to the real container's IP.
  Reachable **only from inside the run's own docker network**. Any port,
  no TLS, no certificate, fastest path, no CA trust needed.
- **`{node}.{workspace}.fghj.internal`** — HTTP(S)-canonical, proxied.
  Resolves to the run's sidecar from inside the network, and to the host
  proxy from the host/a browser outside it — **the same hostname either
  way**. Only ever reachable on **80** (redirects to 443) or **443**
  (terminates TLS with a real, locally-CA-signed cert). Never any other
  port.

Nearly every bug in this area comes from picking the wrong one of these
two for a given call, or from combining the http domain with a raw port
number (which can never work — see [the pitfall](#the-1-pitfall-httpinternal--raw-port-number)
below).

## Decision guide

Ask this about the specific call you're configuring:

**Does anything outside this run's docker network (the host, a browser, a
real `pip`/`npm`/`twine`/`curl` process on your machine, a webhook sender,
an OAuth provider) ever need to dial this exact URL?**

- **No — it's purely container-to-container** (a database connection
  string, an internal API call, a cache, a message broker): use the
  **raw** zone. This is the default and covers nearly every case.
- **Yes** (a presigned S3 URL you hand back to an uploader/downloader, an
  OAuth redirect URI, a webhook callback URL, anything a browser will
  navigate to): use the **http** zone, and make sure the container minting
  that URL also [trusts fghj's CA](#trusting-fghjs-ca-inside-a-container)
  if it dials that same URL itself (not just hands it out).

## The macros

In `.fghj.yaml`, `environment`/`env_file` values can reference a domain
without hand-computing it:

| Token | Zone | Use for |
|---|---|---|
| `${FGHJ_SERVICE_FQDN}` | raw (this node's own) | Rare — a node referencing its own raw address. |
| `${FGHJ_SERVICE_FQDN:name}` | raw (a sibling's) | **The default choice.** `DATABASE_URL`, an internal API base URL, anything container-to-container. |
| `${FGHJ_SERVICE_FQDN_HTTP}` | http (this node's own) | This node minting a URL that points back at itself (e.g. an absolute callback URL for its own OAuth flow). |
| `${FGHJ_SERVICE_FQDN_HTTP:name}` | http (a sibling's) | A sibling's proxied identity — the minio/presigned-URL case below. |
| `${FGHJ_SERVICE_FQDN:a::b::name}` / `_HTTP` variant | either | Disambiguates a `name` that matches more than one sibling, by qualifying it with owning segments, root-first (mirrors the leaf-first node id, just reversed — see [Node identity & domains](/concepts/node-identity-and-domains/)). |

Both macro families resolve a bare `name` the same way: a `kind: backing`
dependency owned by the same service, or (if none matches) a service this
node directly depends on via `kind: service`. **They can't reach a named
port on a sibling** — only its bare domain — so referencing a sibling's
*named* port (e.g. a `management` port declared via `#Port.name`) has to
be a hand-typed literal string. That's exactly where the pitfall below
tends to get introduced.

## Worked example 1: an ordinary internal call (do this by default)

A `php` service talking to its own `mysql` backing dependency — purely
internal, no external consumer ever sees this hostname:

```yaml
environment:
  MYSQL_HOST: ${FGHJ_SERVICE_FQDN:mysql}
```

Nothing else needed. No CA trust, no port suffix (raw ports are supplied
separately, e.g. `:3306`, or the app's own default), works identically
whether `mysql` shares the container network with a Docker-native lookup —
because that's exactly what this is.

## Worked example 2: a sibling's *named* port (can't use the macro)

`aikifactory` declares a second port on itself, named `management`:

```yaml
# aikifactory's own .fghj.yaml
ports:
  "8080":
    primary: true
  "8081":
    name: management
```

A consumer needing that specific port has to hand-type the domain, because
the macro only reaches a sibling's *bare* domain:

```yaml
# the consuming service's .fghj.yaml
environment:
  AIKIFACTORY_MANAGEMENT_URL: http://management.aikifactory.aikifactory.aikido.fghj.raw.internal:8081
```

This is a purely internal call (nothing outside the network needs this
management API), so — per the decision guide above — it's the **raw**
zone, scheme `http://`, and the real container port (`8081`) all
consistently paired together.

### The #1 pitfall: `fghj.internal` + raw port number

The single most common mistake (found three times in one real workspace)
is writing the **http-zone** domain with a **raw port number**:

```yaml
# WRONG — this can never work
AIKIFACTORY_MANAGEMENT_URL: http://management.aikifactory.aikifactory.aikido.fghj.internal:8081
```

The `fghj.internal` suffix always resolves to the sidecar (or host proxy),
and the sidecar/host proxy **only ever listens on 80, 443, and 53** — never
`8081`, never any other raw port. This fails with `ECONNREFUSED
<sidecar-ip>:8081` every single time, deterministically, regardless of
whether routes are fresh or containers were just restarted. It is easy to
misdiagnose as "stale DNS" or "the route table didn't update," because the
IP the domain resolves to looks like it should be wrong — but it's actually
always correct; the *port* is just something that domain's listener can
never serve. **The fix is always the same: switch the domain's suffix from
`fghj.internal` to `fghj.raw.internal`, keep the scheme and port exactly as
they were.**

## Worked example 3: a URL handed to an external caller (needs the http zone + CA trust)

`aikifactory` mints presigned S3 URLs against its `minio` backing
dependency, then hands them back to whoever is uploading/downloading a
package — which might be a real `pip`/`twine`/`npm`/`mvn` process running
on the host, well outside the run's docker network:

```yaml
environment:
  AIKIFACTORY_S3_ENDPOINT: https://${FGHJ_SERVICE_FQDN_HTTP:minio}
```

Why not raw, and why not plain http, here specifically:

- **Raw would break for the external caller.** `fghj.raw.internal` is only
  resolvable from inside the run's own docker network — a presigned URL
  built against it would be unreachable for a real `pip install` running
  on your host.
- **Plain `http://` on the http zone always redirects to `https://`.**
  Port 80 in-zone is a deliberate redirect-only listener for every
  `fghj.internal` name (see [Local CA & TLS
  proxy](/concepts/local-ca-and-tls-proxy/)) — it's never relayed as
  plaintext, because fghj always owns the CA and can always mint a valid
  cert for its own zone, so there's no reason to ever fall back to a
  weaker plaintext path the way there is for a real third-party domain
  under [`additional_hosts`](/reference/fghj-yaml/#additional-hosts).
- **So `https://` on the http zone is the only combination that's
  reachable both ways** — same hostname, same TLS cert, whether the
  request comes from inside the network (via the sidecar) or from the host
  (via the host-side proxy).

The one thing this needs that the raw zone never did: the **container
minting and using this URL** (here, `aikifactory` itself — it doesn't just
hand the URL out, it also uses the same endpoint for its own internal S3
API calls) has to trust fghj's local CA, or its own outbound HTTPS calls to
that domain will fail certificate verification.

## Trusting fghj's CA inside a container

Neither the host proxy nor the sidecar injects CA trust into any
container automatically — this is a known, documented limitation (see
[Local CA & TLS proxy → Limitations](/concepts/local-ca-and-tls-proxy/#limitations)).
If a container needs to *dial* the http zone itself (not just hand out a
URL for someone else to dial), you need to get fghj's CA cert into that
container's trust store yourself.

`fghjd` maintains two stable, world-readable, key-free files for exactly
this, refreshed automatically whenever it starts — no `.fghj.yaml` schema
of its own, just two paths on the host you mount wherever you need them
via the existing generic `volumes:` mechanism:

| File | Contents | Use for |
|---|---|---|
| `/var/lib/fghjd/ca/cert.pem` | just fghj's CA cert | An image with a shell/package manager — point a language-specific trust-store env var at it. |
| `/var/lib/fghjd/ca/bundle.pem` | fghj's CA cert **merged with this host's own real root CA store** | A drop-in replacement for a container's entire system trust file — real CAs stay trusted too, not just fghj's. |

**For an image with a shell/package manager**, mount `cert.pem` and point
the usual language-specific mechanism at it — pick whichever your HTTP
client actually honors:

```yaml
volumes:
  - host: /var/lib/fghjd/ca/cert.pem
    container: /usr/local/share/fghj-ca.pem
    read_only: true
environment:
  NODE_EXTRA_CA_CERTS: /usr/local/share/fghj-ca.pem   # Node
  # REQUESTS_CA_BUNDLE: /usr/local/share/fghj-ca.pem  # Python (requests)
  # SSL_CERT_FILE: /usr/local/share/fghj-ca.pem       # most things on Linux
```

**For a scratch/distroless image with no shell** (common for a statically
compiled Go binary, for example), there's nothing to run *inside* the
container to update trust — but you can mount `bundle.pem` straight over
the one file the runtime's TLS library reads, exactly the same mechanism
you'd use to override any other file baked into an image:

```yaml
volumes:
  - host: /var/lib/fghjd/ca/bundle.pem
    container: /etc/ssl/certs/ca-certificates.crt
    read_only: true
```

No per-repo file to build or keep in sync — `bundle.pem` is a live file
maintained by `fghjd` itself (see `ca::refresh_trust_files`), already
merged with your host's real root CA store, so it's safe even for a
service that also makes genuine external HTTPS calls (real AWS, a real
OAuth provider, etc.) alongside its in-zone ones. If fghj's local CA is
ever regenerated (a fresh `/var/lib/fghjd/ca/` — normally a one-time,
per-machine event that should never happen in ordinary use), this file is
regenerated right along with it, so there's nothing to remember to rebuild.

## Quick reference

| Scenario | Zone | Scheme/port | CA trust needed in the caller? |
|---|---|---|---|
| DB/cache/broker connection string | raw | whatever the backing service speaks | No |
| Internal API call, own or named port | raw | `http://...:<port>` | No |
| This node's own primary port, called internally | raw | `http://<raw-domain>` (primary port implied) | No |
| Presigned URL / OAuth redirect / webhook URL handed to an external caller | http | `https://<http-domain>` (no port — only 80/443 exist) | Yes, if this same container also dials that URL itself |
| `fghj.internal` + a raw port number | — | **never valid** | — |
