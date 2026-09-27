---
title: 4. A second repo
description: A cross-repo dependency, the fog of war before it's pulled, and how two containers reach each other.
sidebar:
  order: 4
---

Everything so far lived in one repo. This is the chapter where fghj starts
doing something Compose can't: a dependency on a repository that has never
heard of yours.

## Declare the dependency before the repo exists

In `storefront/.fghj.yaml`, add a third entry to `web`'s `dependencies:`:

```yaml
      - kind: service
        repo: https://github.com/you/catalog.git
```

That's the whole declaration. No service name, no port, no path — and note
what's *absent*: no branch. Branch is not a property of a dependency edge in
fghj, deliberately. Two repos could otherwise demand different branches of
the same third repo, and there is only one checkout of it on disk. Branch
identity belongs to the workspace checkout, not to the edge pointing at it.
See [Branch ownership model](/concepts/branch-ownership-model/).

Resolve, *before* creating anything:

```bash
cd ~/code/shop
fghj graph https://github.com/you/storefront.git
```

```json
{
  "id": "catalog",
  "label": "catalog",
  "kind": "service",
  "repo": "https://github.com/you/catalog.git",
  "local_path": "catalog",
  "domain": "catalog.shop.fghj.internal",
  "downloaded": false,
  "dirty": false,
  "flows": []
}
```

This is the **fog of war**. fghj knows a node is there, because your repo
said so, and knows nothing else about it — not its service name, not its
ports, not its own dependencies. A repo's dependencies aren't knowable until
that repo is on disk. `downloaded: false` is the UI's cue to draw it as a
placeholder with a pull button.

The stub's id is `catalog`, from the folder name the URL implies. Once the
real repo lands, the id becomes `{its service name}.catalog` and the domain
changes with it — the placeholder is a guess at a node's identity made
without the file that defines it.

In the normal case you'd click **Pull all** here: the daemon clones what's
missing, re-resolves, and repeats until nothing new turns up — a loop,
because each clone can reveal dependencies of its own. We're writing
`catalog` by hand instead, so there's nothing to fetch.

## The second repo

```bash
mkdir -p ~/code/shop/catalog
cd ~/code/shop/catalog
git init
git remote add origin https://github.com/you/catalog.git
```

The folder name is not a free choice: it must be `catalog`, the last segment
of the URL your other repo named. That convention is the entire lookup
mechanism — there's no registry mapping URLs to paths, so the path has to be
derivable from the URL by anyone, offline.

`Dockerfile`:

```docker
FROM node:22-alpine
WORKDIR /app
COPY server.js ./
CMD ["node", "server.js"]
```

`server.js` — a product list, plus a liveness ping to its own Redis:

```js
const http = require('node:http');
const net = require('node:net');

const port = Number(process.env.PORT || 4000);

const ITEMS = [
  { name: 'Reading lamp', price: 42 },
  { name: 'Oak stool', price: 79 },
  { name: 'Wool blanket', price: 55 },
];

// Redis' wire protocol is simple enough to speak by hand, which keeps this
// tutorial free of npm dependencies: send PING, expect +PONG.
function cachePing() {
  return new Promise((resolve) => {
    const socket = net.connect({ host: process.env.REDIS_HOST, port: 6379 });
    socket.setTimeout(2000);
    socket.on('connect', () => socket.write('PING\r\n'));
    socket.on('data', (chunk) => {
      socket.destroy();
      resolve(chunk.toString().trim());
    });
    socket.on('timeout', () => {
      socket.destroy();
      resolve('timeout');
    });
    socket.on('error', (err) => resolve(err.code));
  });
}

http
  .createServer(async (req, res) => {
    if (req.url === '/health') {
      res.writeHead(200).end('ok');
      return;
    }
    if (req.url === '/items') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify(ITEMS));
      return;
    }
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end(`catalog api\ncache says: ${await cachePing()}\n`);
  })
  .listen(port, () => console.log(`catalog listening on ${port}`));
```

And `.fghj.yaml`:

```yaml
version: "1.0"

services:
  api:
    build:
      context: .
    ports:
      "4000":
        primary: true
    environment:
      PORT: "4000"
      REDIS_HOST: ${FGHJ_SERVICE_FQDN:cache}
    dependencies:
      - kind: backing
        name: cache
        image: redis:7
        ports: ["6379"]
        healthcheck:
          test: ["CMD", "redis-cli", "ping"]
          interval: 2
          retries: 15
```

This file mentions `storefront` nowhere. It never will. Dependencies point
one way only, and the pointing repo is the one that has to know anything.

Validate it and resolve again:

```bash
fghj validate .fghj.yaml
cd ~/code/shop
fghj graph https://github.com/you/storefront.git
```

## Five nodes

```
service   id=api.catalog             domain=api.catalog.shop.fghj.internal
backing   id=cache.api.catalog       domain=cache.api.catalog.shop.fghj.internal
backing   id=db.web.storefront       domain=db.web.storefront.shop.fghj.internal
task      id=migrate.web.storefront  domain=migrate.web.storefront.shop.fghj.internal
service   id=web.storefront          domain=web.storefront.shop.fghj.internal
```

```
web.storefront  -> db.web.storefront       owns
web.storefront  -> migrate.web.storefront  owns
api.catalog     -> cache.api.catalog       owns
web.storefront  -> api.catalog             depends-on
migrate.web...  -> db.web.storefront       after
```

The stub is gone, replaced by `api.catalog` — the service is named `api`, in
the folder `catalog`. This is why the service in that repo isn't called
`catalog`: it would have resolved to the id `catalog.catalog`, which is
legal, unambiguous, and reads like a mistake.

`depends-on` is the weaker sibling of `owns`. `web.storefront` needs
`api.catalog` to be up, and that's all: it doesn't name it, didn't create
it, and doesn't destroy it. Two repos can both depend on `api.catalog` and
there is still exactly one of it.

## One dependency, several services

If `catalog` had declared more than one service, `repo:` alone would no
longer name a single node, and the resolver would say so. Then you list what
you want:

```yaml
      - kind: service
        repo: https://github.com/you/catalog.git
        services: ["api", "admin"]
```

One entry per service wanted, still one dependency block. And `repo:` itself
can be omitted entirely to depend on a service declared in *your own* repo's
`services:` map — nothing to clone, no branch to pick, just ordering.

## Reaching across

Give `web` the catalog's address. In `storefront/.fghj.yaml`:

```yaml
      CATALOG_URL: http://${FGHJ_SERVICE_FQDN:api}:4000
```

`${FGHJ_SERVICE_FQDN:api}` resolves by the sibling's **leaf name** — `api`,
the service's own name — and expands to
`api.catalog.shop.fghj.raw.internal`. Raw zone, plain HTTP, port 4000: the
container's real port, one hop, no TLS.

The lookup rules are worth knowing exactly, because they're narrow on
purpose:

- A backing dependency or task owned by the same service as the node whose
  environment this is — that's how `${FGHJ_SERVICE_FQDN:db}` worked.
- Otherwise, a service this node **itself declares** a `kind: service`
  dependency on. Not a transitively-reached one. If you can't name it as a
  dependency, you can't template its address.
- If a leaf name is ambiguous, qualify it root-first with `::`:
  `${FGHJ_SERVICE_FQDN:catalog::api}`.

Then in `storefront/server.js`:

```js
async function catalogItems() {
  const res = await fetch(`${process.env.CATALOG_URL}/items`);
  if (!res.ok) throw new Error(`catalog answered ${res.status}`);
  return res.json();
}
```

and in the handler:

```js
let items;
try {
  items = (await catalogItems())
    .map((item) => `<li>${item.name} — $${item.price}</li>`)
    .join('');
} catch (err) {
  items = `<li>catalog unavailable: ${err.message}</li>`;
}
```

**Start default environment**, and reload:

```
storefront
I answer at https://web.storefront.shop.fghj.internal
database: listening at db.web.storefront.shop.fghj.raw.internal:5432
 • Reading lamp — $42
 • Oak stool — $79
 • Wool blanket — $55
```

Both services are also reachable from your browser under their own HTTPS
names — `https://api.catalog.shop.fghj.internal` shows the catalog's own
page, `cache says: +PONG`. Same node, two addresses, two audiences.

## Why HTTPS works from inside the container too

Every run gets one extra container you didn't declare: a **sidecar** that is
simultaneously the DNS authority for `fghj.internal` inside the run's
network and a TLS proxy identical to the one on your host. Every node in the
run has its `--dns` pointed at it.

So if `web` asked for `https://api.catalog.shop.fghj.internal`, it would get
the sidecar's address, and the sidecar would dispatch by SNI exactly as the
host proxy does — the same hostname means the same thing inside and outside
the network. Anything the sidecar doesn't recognise is forwarded to Docker's
own resolver, which is how raw-zone names and ordinary internet names keep
working.

One catch, and it's why this chapter used plain HTTP over the raw zone
instead: the *container* must also trust fghj's CA for that TLS handshake to
succeed, and `node:22-alpine` has never heard of it. Your host trusts it
because `fghjd` installed it into the system keychain; a container is its own
world. If you need the proxied HTTPS name from inside a container — a
service minting URLs it hands back out, an OAuth redirect, a presigned link
— copy the CA in:

```docker
COPY ca-cert.pem /usr/local/share/ca-certificates/fghj.crt
RUN update-ca-certificates
```

The certificate lives at `/private/var/lib/fghjd/ca/ca-cert.pem`. For plain
service-to-service calls, don't bother — the raw zone is faster and needs
nothing.

---

**Next:** [Flows](/tutorial/05-flows/) — naming the journey.
