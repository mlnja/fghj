---
title: 1. One service
description: A repo, a .fghj.yaml, and a service answering on a real HTTPS domain.
sidebar:
  order: 1
---

We start with one repository and no dependencies at all. By the end of this
chapter a container built from `files-api`'s source is answering at
`https://api.files-api.files.fghj.internal/api/health`.

## The workspace

A **workspace** is just a directory holding sibling repo checkouts. It has
no config file of its own and nothing marks it as special. Its *name*
matters, though, because it becomes part of every domain fghj derives. Call
it `files`:

```bash
mkdir -p ~/code/files
cd ~/code/files
git clone https://github.com/fghj-examples/files-api.git
git -C files-api checkout ch1
```

Everything you do from here on happens either in a repo directory
(`files/files-api`) or in the workspace root (`files`). The CLI's
`--workspace` flag defaults to your current directory, so the habit worth
building is: **run `fghj` from the workspace root.**

Two things about the clone are load-bearing, though you didn't have to do
anything for them:

- A repo's **folder name comes from the last segment of its git URL**:
  `files-api.git` → `files-api`. That convention is how a dependency
  written in one repo finds a checkout on disk in another, with nothing
  central to look it up in.
- The checkout has an **`origin` remote**. fghj won't adopt a directory it
  can't identify. When another repo later declares a dependency on this URL,
  fghj checks that the folder already there really is that repo by comparing
  `git remote get-url origin`, and refuses to guess if it isn't. See
  [Docker & downloads](/concepts/docker-and-downloads/).

## The application

`files-api` at `ch1` is a Node server with one route:

```ts
const app = new Hono().basePath("/api");

app.get("/health", (c) => c.json({ ok: true }));

const port = Number(process.env.PORT ?? 3000);
serve({ fetch: app.fetch, port }, ({ port }) =>
  console.log(`files-api listening on :${port}`),
);
```

and a `Dockerfile` that runs it:

```docker
FROM node:24-alpine
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci --omit=dev
COPY src ./src
EXPOSE 3000
CMD ["node", "src/server.ts"]
```

`node src/server.ts` runs TypeScript directly: Node 24 strips the types and
leaves everything else where it was, so there's no build step and no source
maps. That matters again in chapter 7, when a debugger needs the line
numbers to match.

Note what is *not* here: no port mapping, no hostname, no TLS, no
certificate loading. The app listens on plain HTTP inside its own container
and knows nothing about how the outside world reaches it. That stays true
for the rest of the tutorial.

## The config

`.fghj.yaml`, in the repo root (note the leading dot):

```yaml
version: "2.0"

services:
  api:
    build: .
    ports:
      "3000":
        primary: true
    environment:
      PORT: "3000"
```

Five things are happening in ten lines.

`version: "2.0"`: any `2.x` is accepted. A different *major* is refused
outright; a newer *minor* is accepted and merely noted, so one repo can
adopt a 2.1 feature while its peers stay on 2.0 and they still resolve
together. That asymmetry is what removes the flag day from a federated
config.

`services:` is a **map keyed by service name**, as in Compose. One repo may
declare several containers, and by chapter 3 this one declares six.

`api` is the service's name, and it's a bare label: lowercase letters,
digits, hyphens. It's not a hostname and not an id. fghj derives both from
it.

`build: .` builds from this repo's checkout, with the `Dockerfile` at its
root. There's no `image:` here. A service built from your own source is
built, not pulled.

`ports:` is keyed by **the actual container port number**: `"3000"`,
quoted because YAML would otherwise make it an integer. It's not a semantic
label but the number your app listens on. `primary: true` says "this is the
port to put at the service's own domain". At most one port per service can
be primary.

`environment:` is Compose-shaped, a map or a list of `KEY=value` strings.
There's no `depends_on:`, which means none. The next chapter adds the first.

## Validate it

```bash
fghj validate files-api/.fghj.yaml
```

```
/Users/you/code/files/files-api/.fghj.yaml is a valid component config
```

This shells out to `cue vet` against fghj's own schema, which is why `cue`
is a prerequisite. It's an authoring aid, not the enforcing boundary (the
daemon's own Rust types decide what's acceptable), but it catches typos with
a far better error message than a refused start. See
[fghj validate](/cli/validate/).

## Resolve the graph

Ask what fghj makes of all this:

```bash
fghj graph https://github.com/fghj-examples/files-api.git
```

The entry repo is a *positional* argument. Resolution has to start
somewhere, and any repo will do: there's no root. `files/files-api` already
exists, so nothing is cloned.

```json
{
  "workspace_name": "files",
  "nodes": [
    {
      "id": "api.files-api",
      "label": "api",
      "kind": "service",
      "branch": "HEAD",
      "repo": "https://github.com/fghj-examples/files-api.git",
      "local_path": "files-api",
      "domain": "api.files-api.files.fghj.internal",
      "downloaded": true,
      "dirty": false,
      "head": "f50f03b…",
      "flows": [],
      "build": { "context": ".", "dockerfile": "Dockerfile", "args": {}, "ssh": false },
      "ports": { "3000": { "primary": true, "name": null, "host_port": null, "wildcard": false } },
      "environment": ["PORT=3000"],
      "restart": "no",
      "stop_grace_period": 10,
      "privileged": false
    }
  ],
  "edges": [],
  "warnings": []
}
```

Read the derived fields:

- **`id: "api.files-api"`** is `{service name}.{repo folder name}`. Not just
  `api`, because "api" is a name half the repos in a company will pick.
  Qualifying it with the folder makes it unique without any repo having to
  coordinate.
- **`domain: "api.files-api.files.fghj.internal"`** is the id, then the
  workspace directory's name, then the zone. That's the whole formula, with
  no exceptions. Rename the workspace directory and every domain in it
  changes.
- **`branch: "HEAD"`** is git's own answer for a detached checkout, which
  `git checkout ch1` gives you. fghj is a passive observer of git state: it
  reports the branch, commit and dirtiness it finds, and never switches a
  branch or commits on your behalf. On `main` this would say `main`.

`warnings: []` is worth noticing. Warnings are findings about the graph,
like two primary ports or a duplicate host alias. An empty list means
nothing to look at.

## Wire it into the daemon

`fghj graph` ran entirely in the CLI. To get containers you need the
daemon, which means telling it this workspace exists:

```bash
fghj wire files-api
```

`wire` registers the workspace with `fghjd`, hands over your uid/gid and
`SSH_AUTH_SOCK` (so a root-owned daemon can clone private repos *as you*,
later), and asks it to resolve. From here on the daemon is the one holding
the graph.

The folder you wired sits in `files`, so `files` is the workspace. A URL
works too, `fghj wire https://github.com/fghj-examples/files-api.git`, and
it's what you'd hand a teammate who doesn't have the repo yet: fghj clones
it first.

## Start it

Open `https://fghj.internal/`. That's `fghjd`'s own UI, served over the
same TLS proxy your services will use. Pick the `files` workspace and the
**Actual** tab, click the `api` node, and press **Start** in its drawer.

There's one environment per workspace, and the first Start creates it.
Start is also how you pick up a change later: it recreates the node from
what `.fghj.yaml` and the source say now, first bringing up anything it
depends on that isn't running.

The ☰ next to **Switch to flow** in the header opens the **Activity** log,
which shows the start step by step. In order: a docker
network for the workspace, a sidecar container (more on that in chapter 2),
`docker build` of the repo, then the container, with its port published to
an ephemeral localhost port and a route registered with the proxy for
`api.files-api.files.fghj.internal`.

## Open it

```bash
curl https://api.files-api.files.fghj.internal/api/health
```

```json
{"ok":true}
```

No port number, no `-k`, no certificate warning, and the same in a browser.
Three separate mechanisms had to agree for that to happen:

1. **DNS.** `fghjd` runs an authoritative DNS server for
   `*.fghj.internal` and points your OS's resolver at it for that zone. It
   answers with `127.222.0.1`, a loopback address `fghjd` aliases for
   itself, so your own `127.0.0.1:80` and `:443` stay free for whatever else
   you run. See [Split DNS](/concepts/split-dns/).
2. **TLS.** The connection lands on `fghjd`'s proxy on port 443, which mints
   a leaf certificate for that exact name on the fly, signed by a local CA
   that your system was asked to trust when fghjd was set up. See
   [Local CA & TLS proxy](/concepts/local-ca-and-tls-proxy/).
3. **Dispatch.** The proxy reads the SNI name, finds the route registered
   for it, and relays to the container's published port.

The app did none of this and doesn't know it happened.

---

**Next:** [Two stores](/tutorial/02-two-stores/): the first dependencies,
and the second domain every node has.
