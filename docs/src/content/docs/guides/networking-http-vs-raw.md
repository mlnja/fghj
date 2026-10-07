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

- **`{node}.{workspace}.fghj.raw.internal`** — raw/direct. From inside the
  run's own docker network it resolves straight to the real container IP,
  on any port the container listens on. From the **host** the same name
  also works: fghjd hands out a per-node virtual IP and NATs it to the
  port Docker published, so `db.myworkspace.fghj.raw.internal:5432` is the
  port the author declared, not the one Docker picked (see
  [Node identity & domains](/concepts/node-identity-and-domains/#domain-derivation-one-formula-no-exceptions-two-zones)).
  The host side reaches only **declared** ports, and is a macOS `pf`
  mechanism today. Either way: no TLS, no certificate, fastest path, no CA
  trust needed.
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

- **Raw can't be `https://` at all.** The raw zone goes straight to the
  container — there is no proxy in front of it, so nothing terminates TLS
  and no certificate is ever issued for a `fghj.raw.internal` name. An S3
  client, a browser, or anything else that expects a `https://` endpoint
  has no raw option. (Plain `http://` against the raw zone *would* reach a
  host-side caller on macOS, via the virtual-IP NAT above — it just can't
  be the TLS endpoint this case needs, and it leans on host plumbing the
  http zone doesn't need.)
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

The files you need are **already in the container**. `fghjd` bind-mounts one
directory, read-only, into every container it starts:

| Path in every container | Contents |
|---|---|
| `/etc/fghj/certs/cert.pem` | just fghj's CA cert, PEM-encoded, no key material |
| `/etc/fghj/certs/bundle.pem` | that same cert **merged with this host's own real root CA store** — a complete trust store, not a single cert. Covers the Admin and System trust domains; a CA trusted only in your login keychain [isn't included](/concepts/local-ca-and-tls-proxy/#trust-files-for-containers). |

There is nothing to mount, no path on the host to name, and no `.fghj.yaml`
schema for it. What is left is *activating* them, which is one line of
`environment:`.

### Pick the file by whether you're adding or replacing

This is the whole decision, and getting it backwards is the one way to break
a working service:

- **Replacing** a trust store — `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`,
  overwriting `/etc/ssl/certs/ca-certificates.crt` — means the container now
  trusts *only* what you pointed it at. Use **`bundle.pem`**. Point one of
  these at `cert.pem` and the container trusts fghj and nothing else: every
  genuine external HTTPS call it makes (a real S3, a real OAuth provider)
  starts failing certificate verification.
- **Adding** to a trust store. Only Node has a variable for this
  (`NODE_EXTRA_CA_CERTS`), and a shell can do it by appending. Use
  **`cert.pem`** — appending a copy of the host's roots to a store that
  already has them achieves nothing.

### One environment variable, no shell required

The common case, and the only one that also works for a `scratch` or
distroless image — a statically compiled Go binary, say — because nothing
has to *run* inside the container:

```yaml
environment:
  SSL_CERT_FILE: /etc/fghj/certs/bundle.pem
```

Go, OpenSSL (so `curl`, and anything linked against it), and Python's `ssl`
module all honor `SSL_CERT_FILE`. Python's `requests` reads
`REQUESTS_CA_BUNDLE` instead; set both if you're not sure which path a
library takes.

**Node** is the exception worth knowing, because it's additive — and so it
wants the other file:

```yaml
environment:
  NODE_EXTRA_CA_CERTS: /etc/fghj/certs/cert.pem
```

### Appending into the image's own store

Some things read neither variable and only ever look at the canonical system
path — Java, and a few statically-linked tools. If the image has a shell and
a writable root filesystem, append fghj's cert to the store the image already
has, at startup, before your real process:

```yaml
command: ["sh", "-c", "cat /etc/fghj/certs/cert.pem >> /etc/ssl/certs/ca-certificates.crt && exec my-server"]
```

`>>`, not `>`: appending keeps the real CAs. This needs to run as root, and
it's `cert.pem` rather than `bundle.pem` precisely because it's additive.

**Java honors none of the above** — it reads its own keystore, so it needs an
import rather than a file path:

```yaml
command: ["sh", "-c", "keytool -importcert -noprompt -cacerts -storepass changeit -alias fghj -file /etc/fghj/certs/cert.pem && exec java -jar /app.jar"]
```

### Two things the mount can't do for you

- **It isn't there during a build.** The mount exists for containers, not for
  BuildKit, so a `RUN apt-get` or `RUN npm install` that needs to dial the http
  zone can't read `/etc/fghj/certs`. Use
  [`build.secrets`](/reference/fghj-yaml/#services) to get the cert into a
  build step, or — usually simpler — have the build talk to the raw
  zone over plain `http://`, where no CA is involved at all.
- **It can't help an image with no filesystem at all to write to and no
  environment support.** That's a short list, and `SSL_CERT_FILE` covers
  effectively all of it.

### Why fghj doesn't just turn it on

Mounting is free; activating isn't. As the table above says, nearly every
mechanism Unix offers for trusting an extra CA *replaces* the container's
trust store rather than extending it. Setting `SSL_CERT_FILE` on every
container by default would silently swap each one's idea of the public
internet for fghj's — a change an image's author never asked for, invisible
until some unrelated outbound call fails. So the files are simply there, at a
stable path, costing nothing, for the configs that ask.

The files themselves stay correct on their own: `fghjd` rewrites both on every
start (see `ca::refresh_trust_files`), so if fghj's local CA is ever
regenerated — normally a one-time, per-machine event that should never happen
in ordinary use — the mounted copies follow, with nothing to remember to
rebuild. The host-side originals live in `/var/lib/fghjd/certs/`, separate
from `/var/lib/fghjd/ca/` so that **no private key is ever inside the
directory every container can read**.

## Quick reference

| Scenario | Zone | Scheme/port | CA trust needed in the caller? |
|---|---|---|---|
| DB/cache/broker connection string | raw | whatever the backing service speaks | No |
| Internal API call, own or named port | raw | `http://...:<port>` | No |
| This node's own primary port, called internally | raw | `http://<raw-domain>` (primary port implied) | No |
| Presigned URL / OAuth redirect / webhook URL handed to an external caller | http | `https://<http-domain>` (no port — only 80/443 exist) | Yes, if this same container also dials that URL itself |
| `fghj.internal` + a raw port number | — | **never valid** | — |
