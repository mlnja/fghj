---
title: 1. One service
description: A repo, a Dockerfile, a .fghj.yaml, and a service answering on a real HTTPS domain.
sidebar:
  order: 1
---

We start with one repository and no dependencies at all. By the end of this
chapter a container built from your own source is answering at
`https://web.storefront.shop.fghj.internal`.

## The workspace

A **workspace** is just a directory holding sibling repo checkouts. It has
no config file of its own and nothing marks it as special — but its *name*
matters, because it becomes part of every domain fghj derives. Call it
`shop`:

```bash
mkdir -p ~/code/shop/storefront
cd ~/code/shop/storefront
```

Everything you do from here on happens either in a repo directory
(`shop/storefront`) or in the workspace root (`shop`). The CLI's
`--workspace` flag defaults to your current directory, so the habit worth
building is: **run `fghj` from the workspace root.**

## Make it a git repo, with an origin

```bash
git init
git remote add origin https://github.com/you/storefront.git
```

That URL does not have to exist. Nothing in this tutorial pushes or fetches.
But set it anyway, for two reasons:

- A repo's **folder name comes from the last segment of its git URL** —
  `storefront.git` → `storefront`. That convention is how a dependency
  written in one repo finds a checkout on disk in another, with nothing
  central to look it up in.
- fghj will not adopt a directory it cannot identify. When another repo
  later declares a dependency on this URL and you ask fghj to pull it, it
  checks that the folder already there really is that repo by comparing
  `git remote get-url origin`. With no origin at all you get:

  ```
  ~/code/shop/storefront already exists but is not a git checkout with an
  `origin` remote; fghj cannot confirm it is
  https://github.com/you/storefront.git. Remove or rename it, then pull again.
  ```

  Which is the daemon refusing to guess — see
  [Docker & downloads](/concepts/docker-and-downloads/).

## The application

Two files. First `Dockerfile`:

```docker
FROM node:22-alpine
WORKDIR /app
COPY server.js ./
CMD ["node", "server.js"]
```

Then `server.js` — Node's standard library, nothing else:

```js
const http = require('node:http');

const port = Number(process.env.PORT || 3000);

http
  .createServer((req, res) => {
    if (req.url === '/health') {
      res.writeHead(200).end('ok');
      return;
    }
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
    res.end(`<h1>storefront</h1><p>I answer at ${process.env.OWN_URL}</p>`);
  })
  .listen(port, () => console.log(`storefront listening on ${port}`));
```

Note what is *not* here: no port mapping, no hostname, no TLS, no
certificate loading. The app listens on a plain HTTP port inside its own
container and knows nothing about how the outside world reaches it. That
stays true for the rest of the tutorial.

## The config

Now `.fghj.yaml`, in the repo root (note the leading dot):

```yaml
version: "1.0"

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
```

Six things are happening in fourteen lines:

`version: "1.0"` — any `1.x` is accepted. A different *major* is refused
outright; a newer *minor* is accepted and merely noted, so one repo can
adopt a 1.1 feature while its peers stay on 1.0 and they still resolve
together. That asymmetry is what removes the flag day from a federated
config.

`services:` is a **map keyed by service name**, not a single service. One
repo may build several independent containers from its own source — a
frontend dev-server and a backend process, each with its own Dockerfile.
Most repos declare exactly one, as here.

`web` is the service's name, and it's a bare label: lowercase letters,
digits, hyphens. It is not a hostname and not an id — fghj derives both
from it.

`build.context: .` builds from this repo's checkout. There is no `image:`
here; a service is code you own, so it's built, not pulled. (`dockerfile`
defaults to `Dockerfile`.)

`ports:` is keyed by **the actual container port number** — `"3000"`,
quoted because YAML would otherwise make it an integer. This is not a
semantic label; it's the number your app listens on. `primary: true` says
"this is the port to put at the service's own domain". At most one port per
service can be primary.

`environment:` is Compose-shaped (a map, or a list of `KEY=value`
strings). `${FGHJ_SERVICE_FQDN_HTTP}` is an fghj template, expanded when
the container is created — a node's own domain isn't knowable when you're
writing the file, since it depends on the workspace and the run. Here it
resolves to this service's own proxied domain, so the page can print the
address you used to reach it.

There is no `dependencies:` key, which means none. We add the first one in
the next chapter.

## Validate it

```bash
fghj validate .fghj.yaml
```

```
/Users/you/code/shop/storefront/.fghj.yaml is a valid component config
```

This shells out to `cue vet` against fghj's own schema, which is why `cue`
is a prerequisite. It's an authoring aid, not the enforcing boundary — the
daemon's own Rust types are what actually decide what's acceptable — but it
catches typos with a far better error message than a rejected run. See
[fghj validate](/cli/validate/).

## Resolve the graph

Move up to the workspace root and ask what fghj makes of all this:

```bash
cd ~/code/shop
fghj graph https://github.com/you/storefront.git
```

The entry repo is a *positional* argument: resolution has to start
somewhere, and any repo will do — there's no root. Since
`shop/storefront` already exists, nothing is cloned.

```json
{
  "workspace_name": "shop",
  "nodes": [
    {
      "id": "web.storefront",
      "label": "web",
      "kind": "service",
      "repo": "https://github.com/you/storefront.git",
      "domain_scope": "run",
      "local_path": "storefront",
      "domain": "web.storefront.shop.fghj.internal",
      "downloaded": true,
      "dirty": true,
      "flows": [],
      "build": { "context": ".", "dockerfile": "Dockerfile", "args": {}, "ssh": false },
      "ports": { "3000": { "primary": true, "name": null, "host_port": null, "wildcard": false } },
      "environment": ["OWN_URL=https://${FGHJ_SERVICE_FQDN_HTTP}", "PORT=3000"],
      "restart": "no",
      "stop_grace_period": 10,
      "privileged": false
    }
  ],
  "edges": [],
  "warnings": []
}
```

Read the three derived fields:

- **`id: "web.storefront"`** — `{service name}.{repo folder name}`. Not just
  `web`, because "web" is a name half the repos in a company will pick.
  Qualifying it with the folder makes it unique without any repo having to
  coordinate.
- **`domain: "web.storefront.shop.fghj.internal"`** — the id, then the
  workspace directory's name, then the zone. That's the whole formula, with
  no exceptions. Rename the workspace directory and every domain in it
  changes.
- **`dirty: true`** — read off the checkout: you have uncommitted files
  (all of them). `repo` is the `origin` you set. There's no `branch` or
  `head` yet, because a repo with no commits has neither — fghj is a passive
  observer of git state, so it reports what's there and omits what isn't. It
  never switches a branch or commits on your behalf.

`environment` is still showing the raw template. Templates are expanded
when a container is created, not when the graph is resolved — the run id is
one of the inputs, and at resolve time there is no run.

`warnings: []` is worth noticing. Warnings are advisory findings that
don't stop a resolve (two primary ports, a wildcard on a port that has no
domain, a duplicate host alias). An empty list means nothing to look at.

## Wire it into the daemon

`fghj graph` ran entirely in the CLI. To get containers you need the
daemon, which means telling it this workspace exists:

```bash
fghj wire https://github.com/you/storefront.git
```

`wire` registers the workspace with `fghjd`, hands over your uid/gid and
`SSH_AUTH_SOCK` (so a root-owned daemon can clone private repos *as you*,
later), and asks it to resolve. From here on the daemon is the one holding
the graph.

## Start it

Open `https://fghj.internal/` — that's `fghjd`'s own UI, served over the
same TLS proxy your services will use. Pick the `shop` workspace, then the
**Actual** tab, then **Start default environment**.

The **default run** is the one shared environment per workspace. It tops up
idempotently: press the button again later and it starts whatever's newly
reachable, leaves alone what's already correct, and recreates only what
changed.

Watch the node card go through `creating` and settle on `running`. Behind
it, in order: a docker network for the run, a sidecar container (more on
that in chapter 4), `docker build` of your repo, then the container — with
the port published to an ephemeral localhost port and a route registered
with the proxy for `web.storefront.shop.fghj.internal`.

## Open it

```
https://web.storefront.shop.fghj.internal
```

```
storefront
I answer at https://web.storefront.shop.fghj.internal
```

No port number, no `-k`, no certificate warning. Three separate mechanisms
had to agree for that to happen:

1. **DNS.** `fghjd` runs an authoritative DNS server for
   `*.fghj.internal` and wrote `/etc/resolver/fghj.internal` so macOS sends
   those lookups to it. It answers with `127.222.0.1` — a loopback address
   `fghjd` aliases for itself, so your own `127.0.0.1:80` and `:443` stay
   free for whatever else you run. See [Split DNS](/concepts/split-dns/).
2. **TLS.** The connection lands on `fghjd`'s proxy on port 443, which mints
   a leaf certificate for that exact name on the fly, signed by a local CA
   it installed into your system trust store on first start. See
   [Local CA & TLS proxy](/concepts/local-ca-and-tls-proxy/).
3. **Dispatch.** The proxy reads the SNI name, finds the route registered
   for it, and relays to your container's published port.

Your app did none of this and doesn't know it happened.

---

**Next:** [A database](/tutorial/02-a-database/) — the first dependency,
and the second domain every node has.
