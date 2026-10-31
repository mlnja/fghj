---
title: "OAuth callback URLs on a real HTTPS hostname in local dev"
description: Why OAuth redirect URIs break in local Docker setups (redirect_uri_mismatch, tunnels that change, cookies lost on the callback), and how to give a local app one stable, trusted HTTPS callback URL that every developer registers once.
---

An OAuth or OIDC provider only redirects back to a callback URL registered
with it in advance. Local development makes that URL hard to pin down:

- **`redirect_uri_mismatch`** (or "Invalid parameter: redirect_uri"), because
  this developer runs the app on `localhost:3001` and the console says
  `localhost:3000`.
- **The provider wants `https://`.** Many accept `http://localhost` as an
  exception, but not all, and an enterprise tenant's policy often doesn't.
- **A tunnel URL that changes on every restart**, re-pasted into the
  provider's console, which breaks login for everyone else using that
  client.
- **The login completes and you're logged out anyway.** The `state` or PKCE
  cookie set before the redirect isn't there on the way back: it was
  `Secure` on an HTTP page, or `SameSite=None` without `Secure`, or another
  app on another `localhost` port overwrote it, since cookies aren't
  isolated by port.

The last one is the hardest to spot, and it's the one that disappears in
production, where you're on HTTPS on a real hostname. So the goal is the
same thing locally: **one fixed HTTPS hostname, trusted by the browser,
identical for every developer.**

## The usual ways around it

**Register `http://localhost:<port>`.** Fine for one developer and one
app. It fails with several apps on different ports sharing one cookie jar,
with providers that insist on HTTPS, and with anything that sets `Secure`
cookies.

**A tunnel (ngrok, Cloudflare Tunnel).** You get real HTTPS, and on free
plans a URL that changes. A reserved domain fixes that, for one developer
per domain. Every login round-trips through the internet, and the tunnel
is one more process to keep alive.

**`/etc/hosts` + mkcert + a proxy.** Register `https://app.test/callback`,
point `app.test` at `127.0.0.1`, make a certificate with mkcert, and put a
proxy in front of the app. This is the right shape. It's also a hosts edit,
a certificate and a proxy config per developer, kept in sync by hand. See
[Trusted HTTPS for Docker Compose](/guides/https-localhost-docker-compose/)
for that setup and where it breaks.

## A fixed callback hostname with fghj

fghj gives every service an HTTPS name from its local CA. The derived one,
`web.<repo>.<workspace>.fghj.internal`, contains your workspace's folder
name, so it isn't the same on every machine, and you can't register it in
advance. So the service claims a fixed name as well:

```yaml title=".fghj.yaml"
services:
  web:
    build: .
    ports:
      "80":
        primary: true
    additional_hosts:
      - files.test
```

Register `https://files.test/callback` with the provider **once**, commit
the file, and it works for anyone who runs the repo:

- **It's HTTPS, with a trusted certificate**, because `.test` is a reserved
  TLD ([RFC 2606](https://www.rfc-editor.org/rfc/rfc2606)), so fghj mints
  certificates for it from the CA your machine already trusts. A name under
  a real domain would be proxied over plain HTTP only.
- **It resolves** because fghjd pins exactly that name to its own loopback
  address in a marked block of `/etc/hosts`, while a running container
  claims it, and removes it when the container stops. No other `.test`
  name is touched. See [Resolving additional
  hosts](/concepts/local-ca-and-tls-proxy/#resolving-additional-hosts).
- **Cookies behave like production.** The app is on its own HTTPS origin,
  so `Secure`, `SameSite=None` and `__Host-` cookies are set and come back
  on the callback.

This is exactly how [chapter 4 of the tutorial](/tutorial/04-a-second-repo/#a-name-of-your-own)
does login, with [Descope](https://www.descope.com) as the provider: the
example's Descope project has `https://files.test/callback` registered,
and logging in works on a fresh clone with nothing to configure.

If the app builds its own redirect URI from its hostname, the
`${FGHJ_SERVICE_FQDN_HTTP}` macro expands to the service's own http-zone
name, for providers where a per-developer URL is acceptable:

```yaml title=".fghj.yaml"
environment:
  OAUTH_REDIRECT_URI: https://${FGHJ_SERVICE_FQDN_HTTP}/auth/callback
```

## The server-side half

The redirect is done by the browser, which is on your machine and resolves
`files.test`. The **token exchange** is done by your backend, from a
container, against the provider's real HTTPS endpoint on the internet. Two
things to keep straight there:

- If the backend also has to trust fghj's CA (because it calls another
  local service over `https://`), use `bundle.pem`, which is fghj's CA
  **plus** your machine's real roots. Pointing `SSL_CERT_FILE` at fghj's CA
  alone makes the call to the provider fail verification.

  ```yaml title=".fghj.yaml"
  environment:
    SSL_CERT_FILE: /etc/fghj/certs/bundle.pem
  ```

- If the provider itself runs locally (Keycloak, Dex, a mock OIDC server),
  give it an http-zone name too. Then the browser and the backend use the
  same issuer URL, and the token's `iss` claim matches what the backend
  validates. See [localhost vs.
  host.docker.internal](/guides/localhost-vs-host-docker-internal/) for why
  that otherwise breaks.

## What it doesn't cover

- **Webhooks and back-channel calls from the provider** (SCIM, back-channel
  logout, event hooks) come from the provider's servers, which can't
  resolve a name that exists only on your machine. Those still need a
  tunnel.
- **An alias is a claim on one name across your machine.** Two workspaces
  that both declare `files.test` don't share it: the first running
  container to claim it keeps it, and the other copy is reachable on its
  derived name only.

## Further reading

- [Tutorial chapter 4](/tutorial/04-a-second-repo/): a second repo, one
  origin for the browser, and the login callback.
- [What fghj fixes](/guides/what-it-fixes/): cookies, redirect URIs and
  presigned URLs in one page.
- [`additional_hosts`](/reference/fghj-yaml/#additional-hosts) in the
  `.fghj.yaml` reference.
