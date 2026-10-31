---
title: "localhost vs. host.docker.internal: one URL inside and outside Docker"
description: Why a service has one address for your browser and another for its containers, what host.docker.internal, host networking and /etc/hosts each fix and break, and how to give every service a single URL that works from both sides.
---

A service in Docker Compose has two addresses. Your browser reaches it at
`http://localhost:9000`. Another container reaches it at `http://minio:9000`.
Neither works from the other side:

- Inside a container, `localhost` is **the container itself**, so
  `localhost:9000` refuses the connection.
- On your machine, `minio` is a name only Docker's embedded DNS knows, so
  the browser can't resolve it.

Most of the time you route around this with two variables, `API_URL` for
containers and `API_PUBLIC_URL` for the browser, and it's fine. It stops
being fine the moment one URL has to work for both.

## Where two addresses actually break

**Presigned S3 URLs.** Your app signs a URL against the endpoint it knows,
`http://minio:9000`, and SigV4 signs the `Host` header. Hand that URL to the
browser and it can't resolve `minio`. Rewrite it to `localhost:9000` and
MinIO answers:

```xml
<Error><Code>SignatureDoesNotMatch</Code></Error>
```

**OIDC issuers.** The browser logs in against a Keycloak (or any OIDC
provider) at `http://localhost:8080`, so the token's `iss` claim says
`localhost:8080`. Your API, in a container, validates the token against
`http://keycloak:8080`, and rejects it with an issuer mismatch.

**Links the backend writes down.** A password-reset email, a webhook
registration, a `Location` header, a URL stored in the database. The
backend only knows its in-network name, and the browser only knows the
other one.

**Calls across Compose projects.** The frontend repo's container wants the
API from another repo's Compose project. Its service name doesn't resolve
in your network at all, so you're back to `host.docker.internal` and a
published port.

All of these are the same bug: *one address means different things
depending on who's asking.*

## The usual fixes, and where each one stops

### `host.docker.internal`

It lets a container reach a port **published on your machine**, so the
container calls `http://host.docker.internal:9000` and the browser calls
`http://localhost:9000`. It's the right answer to "my container needs a
service I'm running on the host". It doesn't solve this problem, because
it's still two names: a URL minted with one is wrong for the other side.

On Linux it doesn't even exist until you add it per service:

```yaml title="compose.yaml"
extra_hosts:
  - "host.docker.internal:host-gateway"
```

### Host networking

`network_mode: host` makes `localhost` mean the same thing in both places,
because the container shares your machine's network stack. It also removes
everything Compose networks give you: every service binds your real ports,
so two projects that both want 5432 collide again, and container DNS by
service name is gone. Docker Desktop on macOS only added host networking
recently, and it behaves differently from Linux.

### An `/etc/hosts` line per service

The oldest trick, and it gets surprisingly far:

```text title="/etc/hosts"
127.0.0.1 minio
```

```yaml title="compose.yaml"
services:
  minio:
    ports:
      - "9000:9000"   # host port must equal container port
```

Now `http://minio:9000` resolves to the container inside the network and to
the published port outside it. One URL, both sides. What it costs:

- Every published port must equal its container port, so two projects with
  a Postgres can't both run.
- A hosts edit per service, per teammate, with `sudo`, kept in sync by hand.
- It's plain HTTP. `Secure` cookies, `SameSite=None`, and anything that
  wants a secure context don't work on a name like `minio`.
- A container in a different Compose project still can't resolve `minio`.

### A reverse proxy on `*.localhost`

Traefik or Caddy with `*.localhost` names gives the browser nice URLs, and
`mkcert` can make them HTTPS. The browser side is solved. The container
side isn't: inside a container, `api.localhost` either doesn't resolve or
means the container's own loopback, and neither is your proxy. You can
add a network alias per hostname on the proxy container, and then mount the
mkcert root into every container that dials HTTPS. That works, and it's a
config you now maintain for every service you add. See [Trusted HTTPS for
Docker Compose](/guides/https-localhost-docker-compose/) for that setup in
full.

## What fghj does instead

fghj gives every service a hostname that **resolves from both sides of the
Docker network**, on the same port, with the same certificate:

| | from your browser, `psql`, `curl` | from a container |
|---|---|---|
| `minio.api.shop.fghj.internal` | the host proxy, on 443, trusted cert | the workspace's sidecar, on 443, same cert |
| `db.api.shop.fghj.raw.internal` | a per-service virtual IP, declared port | the container's own IP, same port |

You don't write either name. A macro in `.fghj.yaml` expands to it:

```yaml title=".fghj.yaml"
services:
  api:
    environment:
      S3_ENDPOINT: https://${FGHJ_SERVICE_FQDN_HTTP:minio}
      DATABASE_URL: postgres://app@${FGHJ_SERVICE_FQDN:db}:5432/app
```

`S3_ENDPOINT` is now a URL the API can dial, and the browser can open,
without any rewrite. The proxy never changes the `Host` header, so the
presigned signature MinIO checks is the one your SDK computed.

Three things make it hold where the fixes above don't:

- **Two resolvers, one name.** `fghjd` answers `*.fghj.internal` for your
  machine, and a sidecar in the workspace's network answers it for
  containers. Both terminate TLS with certificates from the same local CA.
  See [Split DNS](/concepts/split-dns/) and [the
  sidecar](/concepts/sidecar/).
- **No published-port bookkeeping.** Each service in the raw zone gets its
  own virtual IP on your machine, forwarded to the port it declared, so
  every Postgres is on 5432 at once. See [Node identity &
  domains](/concepts/node-identity-and-domains/).
- **Across repos.** The sidecar serves every service in the workspace,
  whichever repo it came from, so a container in the frontend repo calls
  the API repo's service by the same name your browser uses. That's
  [chapter 4 of the tutorial](/tutorial/04-a-second-repo/).

If the container that mints an `https://` URL also dials it, it needs to
trust fghj's CA. The files are already mounted in every container; it's
one line of `environment:`, covered in [Trusting fghj's CA inside a
container](/guides/networking-http-vs-raw/#trusting-fghjs-ca-inside-a-container).

## What it doesn't fix

- **Calls from the internet.** A webhook from Stripe or GitHub comes from
  their servers, which can't resolve a name that only exists on your
  machine. That still needs a tunnel.
- **A container that has to reach a process on your host.** That's still
  `host.docker.internal`, and it's the right tool for it.
- **Platforms.** fghj runs on macOS and Ubuntu, not Windows. See
  [Installation](/getting-started/installation/).

## Further reading

- [What fghj fixes](/guides/what-it-fixes/): the same symmetry, applied to
  ports, cookies, redirect URIs and wildcard subdomains.
- [HTTP vs. raw](/guides/networking-http-vs-raw/): which of the two names
  to use for a given call.
- [The tutorial](/tutorial/): a four-repo example app, one chapter at a
  time.
