---
title: 2. Two stores
description: Postgres and S3 next to your service, depends_on, the raw zone and the http zone, healthcheck-gated start order, and volumes that survive.
sidebar:
  order: 2
---

A file service needs two stores: a database for the records and an object
store for the bytes. Neither is code you own, and neither has anything to
build. Each is a **backing service**, just an image. As in Docker Compose,
they're declared next to `api` in `services:`, and `api` says it
`depends_on` them.

```bash
git -C files-api checkout ch2
git -C files-api diff ch1 ch2
```

## Declare them

The two new services in `files-api/.fghj.yaml`:

```yaml
  db:
    image: postgres:16-alpine
    environment:
      POSTGRES_USER: files
      POSTGRES_PASSWORD: dev
      POSTGRES_DB: files
    ports: ["5432"]
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U files"]
      interval: 2
      retries: 15
    stop_grace_period: 30
    volumes:
      - name: pgdata
        container: /var/lib/postgresql/data

  s3:
    image: cgr.dev/chainguard/minio:latest
    command: ["server", "/data", "--console-address", ":9001"]
    environment:
      MINIO_ROOT_USER: files
      MINIO_ROOT_PASSWORD: dev-secret
    ports:
      "9000":
        primary: true
      "9001":
        name: console
    healthcheck:
      test:
        - CMD-SHELL
        - >-
          exec 3<>/dev/tcp/127.0.0.1/9000 &&
          printf 'GET /minio/health/ready HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n' >&3 &&
          read -r line <&3 && case "$line" in *" 200 "*) exit 0;; esac; exit 1
      interval: 2
      retries: 15
    volumes:
      - name: s3data
        container: /data
```

`image:` instead of `build:` is the whole difference between your code and
someone else's image. A service has exactly one of the two.

The two `ports:` look different on purpose. `db` uses the short form, a bare
list of port numbers, each implicitly not primary and not named. That's
right for Postgres: there's no HTTP to route by hostname. `s3` speaks HTTP
on both its ports, so it uses the map form from chapter 1. `9000` is the S3
API and gets the service's own domain. `9001` is MinIO's web console, and
`name: console` gives it a domain of its own, the name in front of the
service's:

```
https://s3.files-api.files.fghj.internal           → port 9000, the S3 API
https://console.s3.files-api.files.fghj.internal   → port 9001, the console
```

Open the second one and log in as `files` / `dev-secret`, and you're looking
at the bucket this chapter's api is about to talk to.

And `api` grows its side of it:

```yaml
  api:
    build: .
    ports:
      "3000":
        primary: true
    environment:
      PORT: "3000"
      DATABASE_URL: postgres://files:dev@${FGHJ_SERVICE_FQDN:db}:5432/files
      S3_ENDPOINT: https://${FGHJ_SERVICE_FQDN_HTTP:s3}
      S3_BUCKET: files
      S3_ACCESS_KEY: files
      S3_SECRET_KEY: dev-secret
      NODE_EXTRA_CA_CERTS: /etc/fghj/certs/cert.pem
    healthcheck:
      test: ["CMD", "wget", "-qO-", "http://localhost:3000/api/health"]
      interval: 2
      retries: 15
    depends_on:
      db: {condition: service_healthy}
      s3: {condition: service_healthy}
```

## The ids, and the edges

```bash
fghj graph https://github.com/fghj-examples/files-api.git
```

Three nodes now. The new `s3` one:

```json
{
  "id": "s3.files-api",
  "kind": "backing",
  "image": "cgr.dev/chainguard/minio:latest",
  "domain": "s3.files-api.files.fghj.internal",
  "ports": {
    "9000": { "primary": true, "name": null, "host_port": null, "wildcard": false },
    "9001": { "primary": false, "name": "console", "host_port": null, "wildcard": false }
  },
  "volumes": [{ "name": "s3data", "container": "/data", "read_only": false, "shared": false }],
  "healthcheck": { "test": ["CMD-SHELL", "exec 3<>/dev/tcp/…"], "interval": 2,
                   "timeout": null, "start_period": null, "retries": 15 }
}
```

`id: "s3.files-api"` is `{name}.{repo folder}`, the same formula as
`api.files-api`. The name `s3` is scoped by the repo that declared it, which
is what lets every repo in the workspace call its database `db` without
collision. `kind: "backing"` is only how it's shown: a service with `image:`
and no `build:`.

And two edges:

```json
{ "from": "api.files-api", "to": "db.files-api", "kind": "depends-on",
  "required": true, "condition": "service_healthy", "flows": [] }
{ "from": "api.files-api", "to": "s3.files-api", "kind": "depends-on",
  "required": true, "condition": "service_healthy", "flows": [] }
```

They're the `depends_on`. `required: true` is the default ("`api` can't
start without it"), so starting `api` always starts both stores first and
waits for them to be healthy. The `${FGHJ_SERVICE_FQDN:db}` in `api`'s
environment adds no edge: a hostname is only an address. Every edge is one
you declared.

## Two zones, and a store in each

Look at how `api` addresses its two stores, because they're deliberately
different.

`DATABASE_URL` uses `${FGHJ_SERVICE_FQDN:db}`, "the raw domain of the
service called `db` in this repo". At container-create time that expands to:

```
postgres://files:dev@db.files-api.files.fghj.raw.internal:5432/files
```

Note the zone: **`fghj.raw.internal`**, not `fghj.internal`. Every node has
a domain in both, and they mean different things:

| Zone | Resolves to | Use it for |
|---|---|---|
| `*.fghj.internal` | fghj's TLS proxy, which dispatches by hostname | anything a browser opens, and any URL you hand to someone outside the container |
| `*.fghj.raw.internal` | the container itself, on its own real port | everything else: databases, brokers, internal API calls, any protocol that isn't HTTP |

The Postgres wire protocol can't go through an HTTP proxy that dispatches
on SNI, so the raw zone is the only answer for `db`. That's also why the
bare `${FGHJ_SERVICE_FQDN}` template gives you the raw domain and the
proxied one needs the longer `_HTTP` spelling: the raw one is what callers
nearly always want.

`S3_ENDPOINT` is the exception, and the reason is in how files get
downloaded. The api never streams a file's bytes itself. It hands the
browser a **presigned URL**, a link straight to the object in S3 with a
signature in the query string, and the browser fetches it from S3 directly.
The S3 client builds that URL from the endpoint it was configured with, and
the signature covers the host. So the endpoint has to be a name the
*browser* can reach:

```
S3_ENDPOINT: https://s3.files-api.files.fghj.internal
```

That's `${FGHJ_SERVICE_FQDN_HTTP:s3}`, the http zone. The raw name would
produce links that work from inside the network and nowhere else, and a
signature over a rewritten host doesn't verify.

Both templates do two things for you:

- You never hand-compute a domain, and a rename can't leave a stale string
  behind.
- The expansion follows the node's id, so moving or renaming `db` moves
  every address that names it.

A template that names nothing, like `${FGHJ_SERVICE_FQDN:bd}`, is a
blocking warning: fghj refuses to start rather than hand `api` an address
that will never resolve. See
[HTTP vs. raw: choosing a zone](/guides/networking-http-vs-raw/) for the
long version.

## The same name from inside the container

The api also talks to S3 itself, to list buckets and later to create one,
over that same `https://s3.files-api.files.fghj.internal`. That works
because every workspace gets one extra container you didn't declare: a
**sidecar** that is the DNS authority for `fghj.internal` inside the
workspace's network, and a TLS proxy identical to the one on your host.
Every node has its DNS pointed at it.

So when `api` looks up `s3.files-api.files.fghj.internal` it gets the
sidecar, and the sidecar dispatches by SNI exactly as the host proxy does.
The same hostname means the same thing inside and outside the network.
Anything the sidecar doesn't recognise goes on to Docker's own resolver,
which is how raw-zone names and the internet keep working.

The container still has to *trust* the certificate it's shown, and
`node:24-alpine` has never heard of fghj's CA. fghj bind-mounts the trust
files into every container it starts, read-only, at `/etc/fghj/certs`:

| File | Holds |
|---|---|
| `cert.pem` | fghj's CA certificate, and nothing else |
| `bundle.pem` | the same certificate merged with your host's root store: a complete trust store |

Nothing reads them until you point something at them. Node's
`NODE_EXTRA_CA_CERTS` *adds* to its built-in roots, so it takes `cert.pem`:

```yaml
      NODE_EXTRA_CA_CERTS: /etc/fghj/certs/cert.pem
```

A runtime whose variable *replaces* its roots (`SSL_CERT_FILE`,
`REQUESTS_CA_BUNDLE`) wants `bundle.pem` instead, or every other HTTPS call
it makes stops working. See
[Trusting fghj's CA inside a container](/guides/networking-http-vs-raw/#trusting-fghjs-ca-inside-a-container).

## Wait for healthy, not for started

The `healthcheck` blocks are Docker's own `HEALTHCHECK`, with intervals in
**seconds** rather than nanoseconds. Their effect in fghj is start ordering:
anything that depends on a node waits for Docker to report it **healthy**
before it starts, not merely created. `condition: service_healthy` in
`api`'s `depends_on` says that's what `api` expects, and fghj checks that
`db` and `s3` really have a healthcheck to wait on.

Without them, `api` would start the instant the Postgres *container*
existed, which is several seconds before Postgres accepts connections.
`pg_isready` is the difference between "the process launched" and "the
database is actually up". MinIO's image ships no `curl` and no `mc`, so its
check speaks HTTP to itself over bash's `/dev/tcp`. Ugly, and it only needs
to work.

`api` has a healthcheck too, although nothing waits on it yet. A
healthcheck belongs to the service that can be checked, not to whoever
happens to depend on it, and it's what the node's health pill in the UI
reads. Chapter 7 comes back to it.

`interval: 2` with `retries: 15` gives each one 30 seconds to come good.
There's a budget for this so one wedged healthcheck can't hold everything
else hostage. See [Concurrency model](/concepts/concurrency-model/).

## The two knobs that protect your data

**`stop_grace_period: 30`**: when the container is stopped, Docker sends
the stop signal and waits this long before `SIGKILL`. The default is 10
seconds, matching Docker. Postgres wants longer: it's flushing to disk, and
being killed mid-write is how a data directory gets corrupted. The value is
stamped onto the container at *create* time rather than passed with each
stop call, so it's honoured even by a plain `docker stop` by hand.
(Its companion, `stop_signal`, overrides which signal is sent. nginx, for
instance, wants `SIGQUIT` for a graceful shutdown.)

**`volumes: [{name: pgdata, ...}]`**: a *named* volume (Docker-managed
storage), not a bind mount. `name: pgdata` is a bare label, like a service
name. The real Docker volume name is derived from it, folding in the
declaring node's id, so `pgdata` in this repo and `pgdata` in another repo
are two different volumes. It survives the container being stopped,
destroyed and recreated, so your data stays put. Deleting the volume itself,
or removing the workspace, is what throws it away.

## Start it and see

Back in the UI's **Actual** tab, press **Start** on `api` again.

Start first walks `api`'s requirements in dependency order and brings up any
that aren't running, then recreates `api` itself. So `db` and `s3` are
created and waited on until healthy, both at once since neither needs the
other, and then `api` is rebuilt (its source changed) and recreated with
the new environment.

This chapter's api has a second route, `/api/status`, which reports what
it can reach:

```bash
curl https://api.files-api.files.fghj.internal/api/status
```

```json
{
  "db": "PostgreSQL 16.… on aarch64-unknown-linux-musl, …",
  "s3": { "endpoint": "https://s3.files-api.files.fghj.internal", "buckets": [] }
}
```

The database answered over the raw zone, through Docker's embedded DNS
with no proxy in the path. A node's raw domain is registered as a network
alias, so it resolves to the real container IP. S3 answered over the http
zone, through the sidecar, with a certificate Node trusted because of
one environment variable.

`buckets: []` is the honest state of a brand-new object store. Nothing has
created one yet. That's what the next chapter is for.

---

**Next:** [Migrations and a seed](/tutorial/03-migrations-and-a-seed/): nodes
that are supposed to exit.
