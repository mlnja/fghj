---
title: 2. A database
description: A backing service, depends_on, the raw zone, healthcheck-gated start order, and a volume that survives.
sidebar:
  order: 2
---

The storefront needs Postgres. Postgres is not code you own and has
nothing to build — it's a **backing service**: just an image. As in Docker
Compose, it's declared next to `web` in `services:`, and `web` says it
`depends_on` it.

## Declare it

In `storefront/.fghj.yaml`:

```yaml
version: "2.0"

services:
  web:
    build:
      context: .
    ports:
      "3000":
        primary: true
    environment:
      PORT: "3000"
      OWN_URL: https://${FGHJ_SERVICE_FQDN_HTTP}
      DATABASE_URL: postgres://shop:dev@${FGHJ_SERVICE_FQDN:db}:5432/shop
    depends_on:
      db: {condition: service_healthy}

  db:
    image: postgres:16
    environment:
      POSTGRES_USER: shop
      POSTGRES_PASSWORD: dev
      POSTGRES_DB: shop
    ports: ["5432"]
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U shop"]
      interval: 2
      retries: 15
    stop_grace_period: 30
    volumes:
      - name: pgdata
        container: /var/lib/postgresql/data
```

`image:` instead of `build:` is the whole difference between your code and
someone else's image. A service has exactly one of the two.

`ports: ["5432"]` is the short form — a bare list of port numbers, each
implicitly not primary and not named. That's right for Postgres: there is
no HTTP to route by hostname. (The map form from chapter 1 is available here
too, for an image that exposes a web console alongside its real port.)

## The id

```bash
cd ~/code/shop
fghj graph https://github.com/you/storefront.git
```

The new node:

```json
{
  "id": "db.storefront",
  "label": "db",
  "kind": "backing",
  "image": "postgres:16",
  "domain": "db.storefront.shop.fghj.internal",
  "ports": { "5432": { "primary": false, "name": null, "host_port": null, "wildcard": false } },
  "environment": ["POSTGRES_DB=shop", "POSTGRES_PASSWORD=dev", "POSTGRES_USER=shop"],
  "volumes": [
    { "name": "pgdata", "scope": "stable", "container": "/var/lib/postgresql/data",
      "read_only": false, "shared": false }
  ],
  "restart": "no",
  "stop_grace_period": 30,
  "healthcheck": { "test": ["CMD-SHELL", "pg_isready -U shop"], "interval": 2,
                   "timeout": null, "start_period": null, "retries": 15 }
}
```

`id: "db.storefront"` is `{name}.{repo folder}`, the same formula as
`web.storefront`. The name `db` is scoped by the repo that declared it,
which is what lets every repo in the workspace call its database `db`
without collision. The domain is that id plus the workspace name, exactly
as before — one formula, no exceptions. `kind: "backing"` is only how it's
shown: a service with `image:` and no `build:`.

A new edge appeared too:

```json
{ "from": "web.storefront", "to": "db.storefront", "kind": "depends-on",
  "required": true, "condition": "service_healthy", "flows": [] }
```

It's the `depends_on`. `required: true` is the default — "`web` can't start
without `db`" — so starting `web` always starts `db` first and waits for it
to be healthy. The `${FGHJ_SERVICE_FQDN:db}` in `web`'s environment, below,
adds no edge: a hostname is only an address. Every edge is one you declared.

## Two addresses, and which one to use

`DATABASE_URL` uses `${FGHJ_SERVICE_FQDN:db}` — "the raw domain of the
service called `db` in this repo". At container-create time that expands to:

```
postgres://shop:dev@db.storefront.shop.fghj.raw.internal:5432/shop
```

Note the zone: **`fghj.raw.internal`**, not `fghj.internal`. Every node has
a domain in both, and they mean different things:

| Zone | Resolves to | Use it for |
|---|---|---|
| `*.fghj.internal` | fghj's TLS proxy (port 443/80), which dispatches by hostname | anything a browser opens, anything needing that exact public-looking HTTPS name |
| `*.fghj.raw.internal` | the container itself, on its own real port | everything else — databases, brokers, internal API calls, any protocol that isn't HTTP |

A Postgres wire protocol connection cannot go through an HTTP proxy that
dispatches on `Host`/SNI, so the raw zone is the only answer here. That's
also why the bare `${FGHJ_SERVICE_FQDN}` template gives you the raw domain
and the proxied one needs the longer `_HTTP` spelling: the raw one is what
callers nearly always want. See
[HTTP vs. raw: choosing a zone](/guides/networking-http-vs-raw/).

Two things follow from using the template rather than typing the address:

- You never hand-compute a domain, and a rename can't leave a stale string
  behind.
- The expansion follows the node's id, so moving or renaming `db` moves
  every address that names it.

A template that names nothing — `${FGHJ_SERVICE_FQDN:bd}` — is a blocking
warning: fghj refuses to start rather than hand `web` an address that will
never resolve.

## Wait for healthy, not for started

The `healthcheck` block is Docker's own `HEALTHCHECK`, with intervals in
**seconds** rather than nanoseconds. Its effect in fghj is start ordering:
anything that depends on this node waits for Docker to report it
**healthy** before it starts — not merely "created". `condition:
service_healthy` in `web`'s `depends_on` says that's what `web` expects;
fghj checks that `db` really has a healthcheck to wait on.

Without it, `web` would start the instant the Postgres *container* existed,
which is several seconds before Postgres accepts connections. `pg_isready`
is the difference between "the process launched" and "the database is
actually up".

`interval: 2` with `retries: 15` gives it 30 seconds to come good. There's a
run-wide budget for this so one wedged healthcheck can't hold a whole
environment hostage — see
[Concurrency model](/concepts/concurrency-model/).

## The two knobs that protect your data

**`stop_grace_period: 30`** — when the container is stopped, Docker sends
the stop signal and waits this long before `SIGKILL`. The default is 10
seconds, matching Docker. Postgres wants longer: it's flushing to disk, and
being killed mid-write is how a data directory gets corrupted. The value is
stamped onto the container at *create* time rather than passed with each
stop call, so it's honoured even by a plain `docker stop` by hand, or by a
container that outlived the daemon that made it. (Its companion,
`stop_signal`, overrides which signal is sent — nginx, for instance, wants
`SIGQUIT` for a graceful shutdown.)

**`volumes: [{name: pgdata, ...}]`** — a *named* volume (Docker-managed
storage), not a bind mount. `name: pgdata` is a bare label, like a service
name: the real Docker volume name is derived from it, folding in the
declaring node's id, so `pgdata` in this repo and `pgdata` in another repo
are two different volumes. It survives the container being stopped,
destroyed and recreated — your test data stays put.

## Restart and see

Back in the UI's **Actual** tab, press **Start** on `web` again.

Start first walks `web`'s requirements in dependency order and brings up any
that aren't running, then recreates `web` itself from what `.fghj.yaml`
says now.

So: `db` is new and gets created, waited on until healthy, and then `web`
is recreated with `DATABASE_URL` in its environment.

Reload `https://web.storefront.shop.fghj.internal`. To have something to
show, add the database check to `server.js`:

```js
const net = require('node:net');

// Deliberately no Postgres driver: this tutorial installs no npm packages.
// Opening a TCP connection is enough to prove the raw-zone address resolves
// and the database is listening on it.
function dbReachable() {
  const url = new URL(process.env.DATABASE_URL);
  const host = url.hostname;
  const dbPort = Number(url.port) || 5432;
  return new Promise((resolve) => {
    const socket = net.connect({ host, port: dbPort });
    socket.setTimeout(2000);
    socket.on('connect', () => {
      socket.destroy();
      resolve(`listening at ${host}:${dbPort}`);
    });
    socket.on('timeout', () => {
      socket.destroy();
      resolve(`no answer from ${host}:${dbPort}`);
    });
    socket.on('error', (err) => resolve(`${host}:${dbPort} — ${err.code}`));
  });
}
```

…and make the handler `async`, printing it:

```js
res.end(
  `<h1>storefront</h1>
   <p>I answer at ${process.env.OWN_URL}</p>
   <p>database: ${await dbReachable()}</p>`,
);
```

Press **Start** on `web` once more — the image changed, so `web`
is rebuilt and recreated — and you get:

```
storefront
I answer at https://web.storefront.shop.fghj.internal
database: listening at db.storefront.shop.fghj.raw.internal:5432
```

The container resolved that name through Docker's own embedded DNS: a
node's raw domain is registered as a network alias, so it resolves to the
real container IP with no proxy in the path at all.

---

**Next:** [A migration](/tutorial/03-a-migration/) — a node that is
supposed to exit.
