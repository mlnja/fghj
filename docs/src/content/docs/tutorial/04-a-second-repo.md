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

In `storefront/.fghj.yaml`, add an `include:` at the top, and one more
entry to `web`'s `depends_on`:

```yaml
version: "2.0"

include:
  catalog: https://github.com/you/catalog.git

services:
  web:
    # ...as before
    depends_on:
      db: {condition: service_healthy}
      migrate: {condition: service_completed_successfully}
      catalog: {}
```

`include:` is the only way a file links to another repo. `catalog` is an
alias — what the rest of this file calls that repo — and `depends_on:
catalog` means "wait until all of it is up". No service name, no port, no
path: you don't know what's in `catalog`, and you don't have to.

There is one more field you could add, and it's worth knowing what it's for:

```yaml
include:
  catalog:
    repo: https://github.com/you/catalog.git
    default_branch: main
```

`default_branch` answers exactly one question: **when fghj clones this repo for
the first time, which branch should it land on so the thing is ready to run?**
You're the one including it, so you're the one who knows — maybe
`catalog`'s default branch is `master`, or maybe `main` is a release branch and
the branch that actually works against your service is `develop`. Say so here
and a teammate who pulls your graph gets a working checkout without having to
be told. Omitted, it clones `main`.

It's used in one place, once: the clone.

```
git clone --branch main --single-branch https://github.com/you/catalog.git
```

That's the whole extent of it. Note `--single-branch` — only that branch is
fetched, so it's a genuine "get me a runnable checkout" instruction and not a
full mirror. After that, `default_branch` is never consulted again:

- A **stub** node (declared, not on disk) reports the declared value as its
  `branch`, because that's the branch it *would* be cloned at. That's the node
  the cloner reads.
- A **real** node (on disk) reports whatever `git` says it's on right now.
  fghj reads your working tree; it doesn't hold the declared value against it.
- **Pulling a repo that's already checked out never changes its branch.** fghj
  verifies the `origin` matches and otherwise leaves your tree alone — it will
  not move you off a branch you're working on.

Which is why two repos can disagree about it harmlessly. One repo including
`catalog` with `default_branch: main` and another with
`default_branch: develop` is not a conflict, because there's exactly **one
checkout per repo, workspace-wide** — whichever include gets there first
clones it, and from then on the only thing that decides the branch is you, in that
directory. If an include could *pin* a branch rather than seed one, two of them
could pin different ones and there'd be no correct answer. That failure mode
isn't resolved here; it's unrepresentable. See
[Branch ownership model](/concepts/branch-ownership-model/).

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
real repo lands, the stub is replaced by the nodes its file declares — the
placeholder stands for a repo whose contents nobody has read yet.

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

fghj finds an included repo by its `origin` remote, not its folder name, so
this folder could be called anything. `catalog` — the URL's last segment —
is just what **Pull** would have named it, and what node ids are built
from.

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
version: "2.0"

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
    depends_on:
      cache: {condition: service_healthy}

  cache:
    image: redis:7
    ports: ["6379"]
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      interval: 2
      retries: 15
```

This file mentions `storefront` nowhere. It never will. An include points
one way only, and the pointing repo is the one that has to know anything.

Validate it and resolve again:

```bash
fghj validate .fghj.yaml
cd ~/code/shop
fghj graph https://github.com/you/storefront.git
```

## Five nodes

```
service   id=api.catalog         domain=api.catalog.shop.fghj.internal
backing   id=cache.catalog       domain=cache.catalog.shop.fghj.internal
backing   id=db.storefront       domain=db.storefront.shop.fghj.internal
task      id=migrate.storefront  domain=migrate.storefront.shop.fghj.internal
service   id=web.storefront      domain=web.storefront.shop.fghj.internal
```

```
web.storefront      -> db.storefront        depends-on
web.storefront      -> migrate.storefront   depends-on
migrate.storefront  -> db.storefront        depends-on
api.catalog         -> cache.catalog        depends-on
web.storefront      -> api.catalog          depends-on  via_flow=catalog
web.storefront      -> cache.catalog        depends-on  via_flow=catalog
```

(plus a `uses` edge for each hostname template, as in chapter 2.)

The stub is gone, replaced by `api.catalog` and `cache.catalog` — the
service named `api` and the one named `cache`, in the folder `catalog`. This
is why the service in that repo isn't called `catalog`: it would have
resolved to the id `catalog.catalog`, which is legal, unambiguous, and reads
like a mistake.

`depends_on: catalog` became one edge per node in `catalog`, each labelled
`via_flow=catalog` — the name you waited on. Two repos can both wait on
`catalog` and there is still exactly one of each node.

## Why not just `catalog/api`?

Because `api` is `catalog`'s business, not yours. Write
`depends_on: [catalog/api]` and fghj refuses it:

```
'web.storefront' names 'catalog/api', which is a service — catalog's services are internal; it publishes none, so name 'catalog' to start all of it
```

If `web` could name `catalog`'s services, `storefront`'s file would go stale
every time `catalog` renamed, split or added one — on every branch. Across
repos, `depends_on` names either a whole repo or a **flow** that repo
publishes: a list of its parts it's willing to be depended on by. `catalog`
publishes none yet, so all of it is what you get. The next chapter fixes
that from `catalog`'s side.

## Reaching across

Give `web` the catalog's address. In `storefront/.fghj.yaml`:

```yaml
      CATALOG_URL: http://${FGHJ_SERVICE_FQDN:catalog/api}:4000
```

`${FGHJ_SERVICE_FQDN:catalog/api}` is "service `api` in the repo included
as `catalog`", and expands to `api.catalog.shop.fghj.raw.internal`. Raw
zone, plain HTTP, port 4000: the container's real port, one hop, no TLS.

So a hostname *can* name another repo's service, when `depends_on` can't.
That's deliberate: an address is the network contract, and production
config has `catalog`'s address in it too. The two rules, exactly:

- A bare name — `${FGHJ_SERVICE_FQDN:db}` — is a service in this repo.
- `alias/name` is service `name` in the repo included as `alias`.

A name that matches nothing is a blocking warning. And a hostname never
makes anything wait: it's drawn as a dotted **uses** edge, `web.storefront
→ api.catalog`. Two services that call each other is normal; if a hostname
meant waiting, they would wait on each other forever.

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
database: listening at db.storefront.shop.fghj.raw.internal:5432
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
