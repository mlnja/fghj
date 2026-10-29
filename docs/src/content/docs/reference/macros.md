---
title: Macros
description: The ${FGHJ_SERVICE_FQDN…} macros that put a service's domain into another service's environment, which zone each one gives you, and the one variable fghj sets itself.
---

A service rarely knows where the others live: the domain depends on the
workspace's folder name, and on which repo the other service is in. The
macros put that domain into an environment variable for you, so a
`.fghj.yaml` never spells one out.

```yaml
services:
  api:
    environment:
      DATABASE_URL: postgres://files:dev@${FGHJ_SERVICE_FQDN:db}:5432/files
      S3_ENDPOINT: https://${FGHJ_SERVICE_FQDN_HTTP:s3}
```

When `api`'s container is created, in a workspace called `files`, that
becomes:

```
DATABASE_URL=postgres://files:dev@db.files-api.files.fghj.raw.internal:5432/files
S3_ENDPOINT=https://s3.files-api.files.fghj.internal
```

## The forms

| Macro | Expands to |
|---|---|
| `${FGHJ_SERVICE_FQDN:name}` | The **raw** domain of service `name` in this repo. |
| `${FGHJ_SERVICE_FQDN:alias/name}` | The **raw** domain of service `name` in the repo [included](/reference/fghj-yaml/#include) as `alias`. This is the one place a file may name another repo's service: an address is the network contract, and production config has it too. |
| `${FGHJ_SERVICE_FQDN}` | This service's own **raw** domain. |
| `${FGHJ_SERVICE_FQDN_HTTP:name}`, `${FGHJ_SERVICE_FQDN_HTTP:alias/name}`, `${FGHJ_SERVICE_FQDN_HTTP}` | The same three, giving the **http** domain instead. |

Every service has a domain in both zones; see [Node identity &
domains](/concepts/node-identity-and-domains/#domain-derivation-one-formula-no-exceptions-two-zones).
The raw one is the shorter macro because it's what most callers want.

## Raw or http

| | `${FGHJ_SERVICE_FQDN…}` (raw) | `${FGHJ_SERVICE_FQDN_HTTP…}` (http) |
|---|---|---|
| Domain | `*.fghj.raw.internal` | `*.fghj.internal` |
| Reaches | the container itself | fghj's proxy: the sidecar from a container, fghjd from your machine |
| Port | the service's own, which you write: `:5432`, `:3000` | none: `http://` is 80, `https://` is 443 |
| Protocol | anything: Postgres, Redis, gRPC, HTTP | HTTP and WebSocket only |
| TLS | none | your choice: none over `http://`, or fghj's CA over `https://`, which the caller has to [trust](/guides/networking-http-vs-raw/#trusting-fghjs-ca-inside-a-container) |
| Recorded in the **Traffic** tab | never: nothing sits between the two containers | yes, over either scheme, while the callee is recording |
| Reachable from a browser | no | yes, the same URL |

Use **raw** for a database, a cache, a broker, and any call you won't need
to watch. Use **http** when something outside the network needs the same
URL (a presigned S3 URL, an OAuth redirect, a webhook callback), or when
you want an HTTP call in the callee's Traffic tab.

The zone decides whether the call goes through fghj; the scheme only
decides whether it's encrypted on the way. The proxy answers a request in
the scheme it came in, and never redirects one to the other. Write
`https://` when a browser or a URL you hand out needs it, and `http://`
when the caller would rather not deal with TLS: both are routed and
recorded the same way.

Two mistakes the table rules out:

- **An http domain with a port**, like
  `http://${FGHJ_SERVICE_FQDN_HTTP:api}:3000`. The proxy listens on 80 and
  443 only, so this never connects.
- **An `https://` http domain behind nginx** without
  `proxy_ssl_server_name on;`. The proxy picks the service by the name sent
  in the TLS handshake, and nginx doesn't send one unless told to, so the
  call fails with a 502. `http://` needs no such line, but then nginx must
  not set `Host` to the browser's name, since that's what plain HTTP is
  routed by. See
  [Reaching the TLS proxy from inside a
  container](/reference/fghj-yaml/#reaching-the-tls-proxy-from-inside-a-container).

The [networking guide](/guides/networking-http-vs-raw/) has the long
version, with worked examples.

## Where they work

In **`environment` and `env_file` values**, and nowhere else. Not in
`command`, `image`, `volumes` or a healthcheck. To use a domain in a
command, put it in the environment and let the shell read it:

```yaml
  migrate:
    image: sqldef/mysqldef
    environment:
      MYSQL_HOST: ${FGHJ_SERVICE_FQDN:mysql}
    command: ["sh", "-c", "mysqldef -uroot -p123 -h \"$MYSQL_HOST\" app < /schema/app.sql"]
```

Written straight into the command, `${FGHJ_SERVICE_FQDN:mysql}` reaches the
shell as it is, and `sh` expands it to an empty string.

They expand when the container is created. The container keeps the value
it got; changing the macro, or renaming the service it names, shows as
drift and is picked up when the container is next recreated.

## Names that match nothing

A macro in `environment` is checked when the graph is resolved. A name
that matches no service, no include, or no service in the included repo is
a **blocking warning**: fghj refuses to start the service rather than hand
it an address that will never resolve. A repo that isn't cloned yet can't
be checked, so its names pass until it is.

A macro in an `env_file` isn't checked ahead of time. If it names nothing,
it's left in the value as written.

A macro names a service, not one of its ports: there's no form for a
named port.

## A macro isn't a dependency

A macro expands to a hostname and nothing more. It doesn't make anything
wait, doesn't pull anything into a run, and isn't drawn as a requirement
in the graph. A service could hardcode the same hostname and fghj couldn't
tell the difference, so the graph doesn't depend on how an address was
spelled.

If a service needs another one, say so in
[`depends_on`](/reference/fghj-yaml/#depends_on): required to start, or
`required: false` for one it only calls at runtime.

## The variable fghj sets

fghj adds one variable to a container's environment, and only when you ask:

| Variable | Set when |
|---|---|
| `FGHJ_DEBUG_WAIT=1` | The **halt at startup** switch is on for a service with [`debug:`](/reference/fghj-yaml/#services). Your image reads it to wait for the debugger before running anything; see [Debugging in containers](/guides/debugging/). |

Nothing else is injected. fghj's CA is mounted as files, at
`/etc/fghj/certs/`, not passed in a variable; pointing your runtime at it
is a line of your own `environment`.
