---
title: 4. A second repo
description: The frontend, a cross-repo dependency, one origin for the browser, and a hostname of your own for a login callback.
sidebar:
  order: 4
---

Everything so far lived in one repo. This is the chapter where fghj starts
doing something Compose can't: a dependency on a repository that has never
heard of yours.

`files-api` grows the rest of its API at `ch4`: login, a file list, uploads
and downloads. The frontend that uses it is a second repo, `files-web`:

```bash
git -C files-api checkout ch4
git clone https://github.com/fghj-examples/files-web.git
git -C files-web checkout ch4
```

Press **Start** on `api` first, so it's running this chapter's code. Start
recreates the node you press it on. A dependency that's already running is
left as it is, so starting `web` alone would leave `api` on chapter 3.

`files-web` is a folder in the workspace with a `.fghj.yaml` at its root, and
that's all it takes to join: the daemon notices the new checkout and adds
its nodes to the graph a moment later. There's nothing to wire.

## Declare the dependency

`files-web/.fghj.yaml`:

```yaml
version: "2.0"

include:
  api: https://github.com/fghj-examples/files-api.git

services:
  web:
    build: .
    ports:
      "80":
        primary: true
    additional_hosts:
      - files.test
    environment:
      API_UPSTREAM: http://${FGHJ_SERVICE_FQDN_HTTP:api/api}
    depends_on:
      api: {}
```

`include:` is the only way a file links to another repo. `api` is an
alias, what the rest of this file calls that repo, and `depends_on: api`
means "wait until all of it is up". No service name, no port, no path:
you don't know what's in `files-api`, and you don't have to.

`files-api`'s file mentions `files-web` nowhere. It never will. An include
points one way only, and the pointing repo is the one that has to know
anything.

There's one more field an include can have, and it's worth knowing what
it's for:

```yaml
include:
  api:
    repo: https://github.com/fghj-examples/files-api.git
    default_branch: main
```

`default_branch` answers exactly one question: **when fghj clones this repo
for the first time, which branch should it land on so the thing is ready to
run?** You're the one including it, so you're the one who knows. Maybe the
repo's default branch is `master`, or maybe `main` is a release branch and
the one that actually works against yours is `develop`. Omitted, it clones
`main`.

It's used in one place, once, the clone:

```
git clone --branch main --single-branch https://github.com/fghj-examples/files-api.git
```

After that it's never consulted again. A repo on disk reports whatever git
says it's on right now, and **pulling a repo that's already checked out
never changes its branch**. fghj checks that the `origin` matches and
otherwise leaves your tree alone. That's why your `git checkout ch4` stands:
nothing fghj does will move you off it.

It's also why two repos can disagree about it harmlessly. One include
saying `main` and another saying `develop` is not a conflict, because
there's exactly **one checkout per repo, workspace-wide**. Whichever include
gets there first clones it, and from then on the only thing that decides the
branch is you, in that directory. If an include could *pin* a branch rather
than seed one, two of them could pin different ones and there'd be no
correct answer. That failure mode isn't handled here, because it can't be
written down. See [Branch ownership model](/concepts/branch-ownership-model/).

## Seven nodes

```bash
fghj graph https://github.com/fghj-examples/files-web.git
```

Any repo can be the entry. Starting from `files-web` walks into `files-api`
through the include, so the graph has all seven nodes: `web.files-web`, and
the six from `files-api`.

The new edges:

```
web.files-web → api.files-api         via_flow=files-api
web.files-web → db.files-api          via_flow=files-api
web.files-web → s3.files-api          via_flow=files-api
web.files-web → migrate-db.files-api  via_flow=files-api
web.files-web → migrate-s3.files-api  via_flow=files-api
```

`depends_on: api` became one edge per node in `files-api`, each labelled
with the repo it came through. Two repos can both wait on `files-api` and
there's still exactly one of each node.

One node isn't there: `seed`. "All of it" means everything that would run
if you started `files-api` on its own, and that doesn't include a seed,
because nothing waits on a seed.

## Why not just `api/api`?

Because `files-api`'s services are `files-api`'s business, not yours. Write
`depends_on: [api/api]` and fghj refuses it:

```
'web.files-web' names 'api/api', which is a service — api's services are internal; it publishes none, so name 'api' to start all of it
```

If `web` could name `files-api`'s services, `files-web`'s file would go
stale every time `files-api` renamed, split or added one, on every branch.
Across repos, `depends_on` names either a whole repo or a **flow** that repo
publishes: a list of its parts it's willing to be depended on by.
`files-api` publishes none yet, so all of it is what you get. The next
chapter fixes that from `files-api`'s side.

## Reaching across

`API_UPSTREAM` is `${FGHJ_SERVICE_FQDN_HTTP:api/api}`: "service `api` in
the repo included as `api`, in the http zone". It expands to
`api.files-api.files.fghj.internal`, the same name your browser would use,
so no port: it's port 80, through the workspace's sidecar. The sidecar
answers in whatever scheme it's asked in, so plain `http://` is fine here,
and nginx doesn't have to deal with TLS.

The raw name, `${FGHJ_SERVICE_FQDN:api/api}` plus `:3000`, would work too,
one hop shorter. The http one is here for what it lets you see: a call
through the sidecar is a call fghj can record, and a raw one goes straight
from container to container with nothing in between. There's more on that
[below](#watching-the-calls).

So a hostname *can* name another repo's service, where `depends_on` can't.
That's deliberate: an address is the network contract, and production config
has the api's address in it too. The two rules, exactly:

- A bare name, `${FGHJ_SERVICE_FQDN:db}`, is a service in this repo.
- `alias/name` is service `name` in the repo included as `alias`.

A name that matches nothing is a blocking warning. And a hostname never
makes anything wait, and isn't drawn in the graph at all. Two services
calling each other is normal, and if a hostname meant waiting, they'd wait
on each other forever. The [Macros](/reference/macros/) page has every
form and rule.

nginx is what uses the upstream. `files-web`'s image is the built React app
plus this config, which the nginx image fills in from the environment at
startup:

```nginx
server {
  listen 80;
  root /usr/share/nginx/html;

  location /api/ {
    proxy_pass ${API_UPSTREAM};
  }

  location / {
    try_files $uri /index.html;
  }
}
```

So the browser only ever talks to one origin, the frontend's. `/api/` goes
through to the api, with no CORS and no second hostname for the frontend to
be configured with.

The sidecar serves every service from one address and tells them apart by
the `Host` header. nginx sends the name from `proxy_pass` as `Host`, which
is exactly right, so the one thing not to add is the common
`proxy_set_header Host $host;`: that would send `files.test`, and the
sidecar would route the call back to `web`. Proxying to `https://` works
too, but needs [one more
line](/reference/fghj-yaml/#reaching-the-tls-proxy-from-inside-a-container),
because nginx doesn't send the server name in the TLS handshake unless
told to.

File bytes are the one exception to the single origin,
and that's on purpose: a download is a presigned link to
`s3.files-api.files.fghj.internal`, which chapter 2 made sure the browser
can reach.

## A name of your own

`web`'s derived domain is `web.files-web.files.fghj.internal`, and it has a
problem: the last-but-two segment is *your workspace's folder name*. Call
the folder `files2` and the domain changes with it.

Usually that's fine. Here it isn't, because login is done by
[Descope](https://www.descope.com), and an OAuth provider only redirects back
to callback URLs registered with it in advance. A URL with somebody's folder
name in it can't be registered in advance. So `web` claims a hostname of
its own as well:

```yaml
    additional_hosts:
      - files.test
```

`files.test` routes to the same port as the derived domain, and it's the
same everywhere. `https://files.test/callback` is registered with the
example's Descope project once, and it works for anyone who runs the
example.

Two details make it work:

- **It's HTTPS** because `.test` is a reserved TLD (RFC 2606), so fghj
  mints certificates for it from its CA, as it does for `*.fghj.internal`.
  A name under a real TLD would be proxied over plain HTTP only. fghj's CA
  is trusted by your whole machine, and it won't sign for a domain somebody
  else could own.
- **It resolves** because fghjd pins exactly that name in a marked block of
  `/etc/hosts` while a running container claims it. It doesn't take over the
  whole `.test` suffix, so every other `.test` name is left alone. See
  [Resolving additional hosts](/concepts/local-ca-and-tls-proxy/#resolving-additional-hosts).

An alias is a claim on one name across your machine, not just in this
workspace. Run a second copy of the example in another workspace and both
declare `files.test`. The first running container to claim it keeps it,
and the second copy is reachable on its derived domain only.

## Start it

Press **Start** on `web`. Its dependencies are already up, so this builds
the frontend and creates one container. Then open:

```
https://files.test
```

Log in, and you land on your files. The four samples from the seed are
there, visible to everyone because their owner is null. Upload something of
your own and it appears in the list. Download it, and the browser fetches it
from `s3.files-api.files.fghj.internal` with the signature the api put in
the link.

The derived name, `https://web.files-web.files.fghj.internal`, serves the
same app, but you can't log in there. The frontend asks Descope to send you
back to `/callback` on whatever origin it's served from, and Descope refuses
a callback that isn't registered.

## Watching the calls

Because nginx reaches the api by its http name, every `/api/` call passes
through the sidecar, and fghj can record it. Click `api`, open its
**Traffic** tab, press **⏺ Record**, and use the app: the calls show up as
nginx made them. [Chapter 8](/tutorial/08-capturing-traffic/) is about that
tab.

---

**Next:** [Flows](/tutorial/05-flows/): repos you haven't cloned yet, and
starting less than everything.
