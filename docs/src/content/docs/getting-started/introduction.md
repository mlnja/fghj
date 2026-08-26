---
title: Introduction
description: What fghj is, the problem it solves, and how its pieces fit together.
---

`fghj` is a local development orchestrator for testing **user flows** that
span multiple repositories — a checkout journey that touches a frontend, an
API, a payments service, and a database, say — without hand-maintaining a
shared `docker-compose.yml` or paying the resource cost of a local
Kubernetes cluster.

## The problem

Once a product is split across more than a handful of services, "run it
locally" stops being simple. Two options dominate, and both have a real
cost:

- **One giant Compose file** someone owns and everyone edits — it drifts
  from what any single team actually needs, and it starts everything,
  whether or not the change you're testing touches it.
- **A local Kubernetes cluster** (`kind`/`minikube`) — accurate, but heavy:
  slow to start, resource-hungry, and its own layer of YAML to maintain.

`fghj` takes a third path: **product-centric, not infrastructure-centric**
orchestration. Instead of "everything the org owns," you declare "everything
*this user journey* needs," and `fghj` resolves exactly that subgraph.

## How it's structured

- **Federated config.** There's no root repo or central manifest. Every
  repository carries its own `fghj.yaml`, declaring its own build, its own
  ports, and the dependencies (other services, or backing infrastructure
  like Postgres/Redis) it needs. See [fghj.yaml](/reference/fghj-yaml/) and
  [Flat workspace model](/concepts/flat-workspace-model/).
- **Flows.** Any repo can declare a **flow** — a named user journey and the
  extra dependencies it pulls in beyond the repo's own baseline set. Flows
  are a highlight over the graph, not a hard boundary — see
  [Fog-of-war visibility](/concepts/fog-of-war-visibility/).
- **The superdaemon, `fghjd`.** One root-owned background process per
  machine — modeled on `dockerd` — that owns Docker, a local certificate
  authority, a `*.fghj.internal` DNS zone, and a TLS-terminating reverse
  proxy, so every service you run gets a real, trusted HTTPS domain with no
  port juggling. See [Local CA & TLS proxy](/concepts/local-ca-and-tls-proxy/)
  and [Split DNS](/concepts/split-dns/).
- **The CLI, `fghj`.** The unprivileged command you run day to day —
  `wire` a workspace into the daemon, `validate` a config file, or inspect
  the resolved `graph`. It never touches Docker or ports 80/443 directly;
  everything privileged goes through `fghjd`'s control API. See
  [Control API](/concepts/control-api/).
- **A web UI.** `fghjd` serves a small Svelte app for browsing the resolved
  graph, starting/stopping runs, and tailing container logs. See
  [UI architecture](/concepts/ui-architecture/).

## Where to go next

New to `fghj`? Start with [Installation](/getting-started/installation/),
then [Quickstart](/getting-started/quickstart/) to wire your first
workspace. Curious about a specific design decision? The
[Concepts](/concepts/architecture/) section is written like a subsystem
guide series — each page covers one piece end to end, including the
non-obvious reasons behind it.
