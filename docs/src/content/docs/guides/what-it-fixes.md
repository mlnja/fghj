---
title: What fghj fixes
description: The routing symmetry at the centre of fghj, and six things it removes from local multi-service development — each with the failure mode it replaces.
---

Everything on this page follows from one property, so it's worth stating
first on its own.

## One name, inside and out

Every node gets two hostnames, and **both resolve from either side of the
docker network**:

| | from your browser, `psql`, `curl` | from a container in the run |
|---|---|---|
| `minio.api.shop.fghj.internal` | the host proxy, on 443, trusted cert | the run's sidecar, on 443, same cert |
| `db.web.shop.fghj.raw.internal` | a virtual IP NAT'd to the declared port | the container's own IP, same port |

The hostname does not change. The port does not change. The scheme does not
change. Nothing needs to know which side of the network boundary it's on.

That sounds like a small thing and isn't. Almost every irritation in local
multi-service development is some version of *this address means two
different things depending on who's asking* — which is why you end up with
an `API_URL` and an `API_PUBLIC_URL`, a `MINIO_ENDPOINT` and a
`MINIO_BROWSER_ENDPOINT`, and a rewrite somewhere in the middle that works
until it doesn't.

Two mechanisms produce the symmetry, one per zone:

- **The http zone** (`*.fghj.internal`) is answered by `fghjd`'s own DNS
  server for the host, and by the run's sidecar for containers inside the
  network. Both terminate TLS on 443 with certificates from the same local
  CA. See [Split DNS](/concepts/split-dns/) and [In-network TLS proxy
  sidecar](/concepts/sidecar/).
- **The raw zone** (`*.fghj.raw.internal`) is a real Docker network alias
  inside the network, so it resolves to the container directly. From the
  host, `fghjd` answers with a per-node virtual IP out of a `/16` it owns
  and NATs it to the port the author declared. See [Node identity &
  domains](/concepts/node-identity-and-domains/).

Which one to use for a given call is a short decision, covered in [HTTP vs.
raw](/guides/networking-http-vs-raw/).

## A presigned URL you can paste into a browser

Your app signs an S3 URL against the endpoint it knows, which inside a
compose network is `http://minio:9000`. SigV4 covers the `Host` header. So
the URL you hand to the browser names a host the browser can't resolve — and
the moment you rewrite it to `localhost:9000`, the signature no longer
matches what you signed:

```xml
<Error><Code>SignatureDoesNotMatch</Code></Error>
```

The usual workaround is two endpoints, two clients, and a rewrite somewhere
in the middle.

With fghj there is one name, and it works from both sides:

```yaml title=".fghj.yaml"
environment:
  S3_ENDPOINT: https://${FGHJ_SERVICE_FQDN_HTTP:minio}
```

The proxy is also a byte relay that **never rewrites the `Host` header** —
on the HTTPS path it routes on SNI and never parses the request at all — so
the signature your SDK computed is exactly the signature MinIO verifies.

The one thing this case needs that a purely internal call doesn't: if the
container minting the URL also *dials* that endpoint itself, it has to
[trust fghj's CA](/guides/networking-http-vs-raw/#trusting-fghjs-ca-inside-a-container).

## Every Postgres on 5432. At the same time.

First one is `5432:5432`. The second is `5433:5432`. The third is `55432`
because Docker picked it. Now your `.env`, your GUI client's saved
connections and your teammate's notes all disagree, and you keep a mental
lookup table of which number means which database.

fghj publishes nothing you have to remember. Each raw node gets its own
virtual IP, NAT'd to the port the author *declared* — so the canonical port
is the port, for all of them at once:

```bash
psql -h db.web.storefront.shop.fghj.raw.internal -p 5432
psql -h db.api.billing.shop.fghj.raw.internal    -p 5432
psql -h db.web.storefront.pr-123.shop.fghj.raw.internal -p 5432
```

Three different databases, three live connections, all on 5432. The third is
in a [review run](/tutorial/06-two-runs/) standing beside the other two. The
port went back to being a property of the protocol, and the hostname carries
the identity.

## Redirect URIs that don't change every restart

A tunnel hands you a new URL on every boot, which you re-paste into the
provider's console, which breaks it for everyone else testing against it.

fghj's names are derived and stable, served over HTTPS with a certificate
your system already trusts — so they're valid redirect URIs as they are:

```yaml title=".fghj.yaml"
environment:
  OAUTH_REDIRECT_URI: https://${FGHJ_SERVICE_FQDN_HTTP}/auth/callback
```

And when something already has a hostname on file that you can't change, the
service can answer on that too:

```yaml title=".fghj.yaml"
additional_hosts:
  - app.acme.local
```

Because `.local` is a reserved, never-real TLD — as are `.test`, `.internal`
and `.localhost` — that alias gets a certificate too, though only while a
running container actually claims the route. An alias under a *real* public
suffix (`app.acme.io`) is still routed, but only ever over plain HTTP: fghj
will not mint browser-trusted certificates for names that exist on the
internet.

:::note[Browser redirects, not server-to-server]
This works because the *browser* performs the OAuth redirect, and your
browser is on the machine that can resolve these names. A webhook delivered
from a third party's servers still can't reach a local-only hostname; for
that you still need a tunnel.
:::

## Cookies that behave like production

Cookies aren't scoped by port — [RFC 6265 §8.5](https://www.rfc-editor.org/rfc/rfc6265#section-8.5)
is explicit that they "do not provide isolation by port". So
`localhost:3000` and `localhost:3001` share one jar, and your storefront's
session cookie quietly overwrites your admin app's. Meanwhile `Secure`,
`SameSite=None` and the `__Host-` prefix can't be exercised over plain HTTP
at all, so the auth flow you tested is not the auth flow you ship.

Under fghj every service has a distinct hostname under its own parent
domain, over real TLS. Cookie jars separate the way they do in production,
`__Host-` and `Secure` work, cross-site `SameSite=None` is testable, and
CORS preflight sees genuinely distinct origins.

## Wildcard subdomains, each with a real certificate

A multi-tenant app that routes on subdomain normally means an `/etc/hosts`
edit per tenant. A static wildcard cert doesn't rescue you either: X.509
wildcards match exactly one leftmost label ([RFC 6125](https://www.rfc-editor.org/rfc/rfc6125)),
so `*.fghj.internal` could never cover a name like
`acme.app.shop.fghj.internal`.

fghj mints a leaf certificate per hostname, on first sight:

```yaml title=".fghj.yaml"
ports:
  "3000":
    primary: true
    wildcard: true
```

```
https://acme.app.shop.fghj.internal                ✓ trusted
https://globex.app.shop.fghj.internal              ✓ trusted
https://whatever-you-type.app.shop.fghj.internal   ✓ trusted
```

No `/etc/hosts` edit, no certificate to regenerate, no restart.

## It doesn't take port 80

Every other local-proxy setup binds `0.0.0.0:80`, and now the project you
*weren't* working on can't start its own server.

`fghjd` binds 80 and 443 on `127.222.0.1` — a loopback alias it adds for
itself — and never on `127.0.0.1` or `0.0.0.0`. Your nginx keeps serving and
`curl http://127.0.0.1/` still reaches it. The address sits inside
`127.0.0.0/8` deliberately: [RFC 1122](https://www.rfc-editor.org/rfc/rfc1122)
forbids a loopback-destined packet from ever reaching the wire, so unlike a
`10.x` alias it cannot collide with your VPN, your LAN, or your office
network.

## And while you're in there

| | |
|---|---|
| **Migrations as graph nodes** | [`kind: task`](/concepts/terminating-nodes/) runs to completion and *gates* the services that need it — ordered against the database it seeds, recorded, re-runnable. Not a command you remember to type after everything boots. |
| **Two environments side by side** | A [named run](/tutorial/06-two-runs/) gets its own network, containers, sidecar and domains. Review a PR without tearing down what you were already doing. |
| **Fog-of-war graph** | The graph shows [what's actually on disk](/concepts/fog-of-war-visibility/). Selecting a flow highlights its slice and dims the rest — it never hides it, so you can always see what you'd pull in next. |
| **Federated config** | Every repo declares its own dependencies in [its own `.fghj.yaml`](/concepts/flat-workspace-model/). Peers, not a tree with one root, and no central file to merge-conflict over. |
| **WebSockets and SSE** | Pass straight through. After routing on SNI the proxy relays bytes and rewrites no headers. |
| **Leaves no trace** | [`sudo fghj uninstall`](/cli/uninstall/) reverses all of it: the trusted root CA, the resolver file, the loopback aliases, the pf rules, `/var/lib/fghjd`. |
