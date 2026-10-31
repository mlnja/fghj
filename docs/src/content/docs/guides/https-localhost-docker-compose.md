---
title: "Trusted HTTPS on localhost for Docker Compose services"
description: How to get browser-trusted HTTPS for local Docker Compose services with mkcert and a reverse proxy, the four places that setup breaks (nested names, containers, port 443, teammates), and how fghj issues a trusted certificate per hostname instead.
---

Plain HTTP on `localhost:3000` is enough until it isn't. These only work
over HTTPS on a name that isn't `localhost`:

- **`Secure` cookies**, and so `SameSite=None`, which requires `Secure`,
  and the `__Host-` prefix.
- **Secure-context APIs** (service workers, `crypto.subtle`, clipboard,
  WebAuthn) on any hostname other than `localhost` itself.
- **OAuth providers** that accept only `https://` redirect URIs. See
  [OAuth callbacks on a real HTTPS hostname](/guides/oauth-callback-localhost/).
- **Separate origins.** Cookies aren't isolated by port
  ([RFC 6265 §8.5](https://www.rfc-editor.org/rfc/rfc6265#section-8.5)),
  so `localhost:3000` and `localhost:3001` share a cookie jar. Two
  hostnames don't.

A self-signed certificate gets you HTTPS and a browser warning, and every
client in every container then needs `-k`, `verify=False` or
`NODE_TLS_REJECT_UNAUTHORIZED=0`. That's the auth path you test, and not
the one you ship.

## The standard setup: mkcert and a reverse proxy

The well-trodden route is a local certificate authority, a certificate for
your local names, and a proxy that terminates TLS in front of Compose.

```bash
mkcert -install                          # add a local CA to your trust store
mkcert "*.shop.localhost" shop.localhost # a cert for your names
```

```yaml title="compose.yaml"
services:
  proxy:
    image: traefik:v3
    command:
      - --providers.docker
      - --entrypoints.websecure.address=:443
      - --providers.file.filename=/etc/traefik/tls.yaml
    ports:
      - "443:443"
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
      - ./certs:/certs:ro
      - ./tls.yaml:/etc/traefik/tls.yaml:ro

  api:
    build: .
    labels:
      - traefik.http.routers.api.rule=Host(`api.shop.localhost`)
      - traefik.http.routers.api.tls=true
```

Open `https://api.shop.localhost` and the padlock is green. (Caddy with
`tls internal` does the same with less configuration, and its own CA in
place of mkcert's.) For one browser talking to one Compose project, this is
a good setup and you may need nothing else.

## Where it breaks

### Nested names

The certificate covers `*.shop.localhost`. X.509 wildcards match **exactly
one** leftmost label ([RFC 6125](https://www.rfc-editor.org/rfc/rfc6125)),
so `admin.api.shop.localhost` or a tenant subdomain like
`acme.app.shop.localhost` is a certificate error. Every new depth is a new
`mkcert` run and a proxy reload.

### Containers calling each other over HTTPS

The browser trusts mkcert's CA. Your containers don't: their trust store
came from the image. So when the API calls `https://auth.shop.localhost`,
it fails certificate verification, and before that it fails to resolve
the name, because inside a container `*.localhost` either doesn't resolve
or means the container's own loopback. The fix is per service, by hand:

- a network alias on the proxy for every hostname a container dials,
- the mkcert root mounted into every container,
- and the right variable per runtime to use it (`SSL_CERT_FILE`,
  `REQUESTS_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS`, a Java keystore import).

Get the variable wrong (point `SSL_CERT_FILE` at mkcert's root alone) and
the container now trusts *only* your local CA, so its calls to the real
internet start failing instead.

### Port 443 belongs to one project

The proxy publishes `0.0.0.0:443`. The second project with the same setup
can't start its proxy, so you run one shared Traefik for everything, and
now it's infrastructure with its own config repo.

### Every teammate does it again

`mkcert -install` per machine, certificate files that aren't committed
(they contain a private key), a `tls.yaml`, labels in every service. A new
service means touching the proxy config. A new teammate means an afternoon.

## What fghj does instead

fghj runs the CA and the proxy itself, and issues certificates on demand:

- **A leaf certificate per hostname, minted on the first TLS handshake.**
  fghjd reads the SNI name, checks it's one of its own, and signs a
  certificate for exactly that name. Any depth works, with nothing to
  regenerate. See [Local CA & TLS proxy](/concepts/local-ca-and-tls-proxy/).
- **Trusted once, at install.** On macOS your first `fghj wire` asks you to
  trust the root. On Ubuntu fghjd adds it to the system store and to your
  browsers' NSS databases. The CA that signs leaves is name-constrained to
  fghj's own zones and reserved TLDs, so it can't sign for a real domain.
- **The same certificate inside the network.** A sidecar in each
  workspace's network answers the same hostnames for containers and
  presents certificates from the same CA, so
  `https://auth.api.shop.fghj.internal` is valid from your browser and from
  a container. See [the sidecar](/concepts/sidecar/).
- **The CA files are already in every container**, at
  `/etc/fghj/certs/cert.pem` (fghj's CA alone) and
  `/etc/fghj/certs/bundle.pem` (fghj's CA plus your machine's real roots).
  Turning them on is one line:

  ```yaml title=".fghj.yaml"
  environment:
    SSL_CERT_FILE: /etc/fghj/certs/bundle.pem
  ```

  See [Trusting fghj's CA inside a
  container](/guides/networking-http-vs-raw/#trusting-fghjs-ca-inside-a-container)
  for Node, Java and `requests`.
- **It doesn't take your port 443.** fghjd binds 80 and 443 on
  `127.222.0.1`, a loopback alias it adds for itself, never `0.0.0.0` or
  `127.0.0.1`. Your own nginx keeps working.

There's no certificate in the repo and no proxy config. A service declares
its port, and its name follows:

```yaml title=".fghj.yaml"
services:
  api:
    build: .
    ports:
      "3000":
        primary: true
```

That's `https://api.<repo>.<workspace>.fghj.internal`, trusted, from both
sides. For a fixed name, such as a callback URL you register with an OAuth
provider, add an alias under a reserved TLD and it gets a certificate too:

```yaml title=".fghj.yaml"
    additional_hosts:
      - shop.test
```

## Limits

- fghj only issues certificates for its own zones and reserved TLDs
  (`.test`, `.local`, `.internal`, `.localhost`). An alias under a real
  domain is routed over plain HTTP only. That's deliberate: a CA your whole
  machine trusts shouldn't sign for names someone else can own.
- On Linux, only the Debian family's trust store is implemented (Debian,
  Ubuntu and derivatives). Windows isn't supported.
- The CA files are mounted into containers, not into `docker build`. A
  `RUN` step that needs HTTPS to a local service needs `build.secrets` or
  the raw zone.

## Further reading

- [localhost vs. host.docker.internal](/guides/localhost-vs-host-docker-internal/):
  one URL from the browser and from containers.
- [What fghj touches, and how to remove it](/guides/uninstalling/): the CA,
  the resolver files and the loopback alias, and the one command that
  removes them.
- [Quickstart](/getting-started/quickstart/): from install to a service on
  HTTPS.
