---
title: Dependency kinds
description: Needed to start vs needed at runtime — what `required` in depends_on does to ordering, waiting, failure, restarts and the graph.
---

Every edge between two services is declared, in `depends_on`. An edge has one
of two kinds, and the kind is the whole of what fghj does with it.

```yaml
services:
  api:
    build: .
    depends_on:
      db:      {condition: service_healthy}  # required: true is the default
      migrate: {condition: service_completed_successfully}
      hooks:   {required: false}             # called at runtime, not at boot
```

## The two kinds

| | `required: true` (default): **needed to start** | `required: false`: **needed at runtime** |
|---|---|---|
| Graph | solid line | dashed line |
| Which runs start it | every run that starts the dependent (the required closure) | only a run whose flows list it |
| Order | the dependency starts first | none |
| Waiting | the dependent waits until it's ready: healthy if it has a healthcheck, exited 0 if it's a task, running otherwise | none |
| It fails to start | the dependent isn't started: "blocked by db", transitively | nothing |
| It's recreated | the dependent is restarted after it | nothing |
| It's stopped | the dependent is stopped first | nothing |
| Starting the dependent alone | also starts it if it isn't running | nothing |
| A cycle | blocking: it can never start | can't happen: never part of a cycle |
| Not in the run | can't happen | advisory before start: "api needs hooks at runtime, but this run doesn't start it" |

"Required" answers one question: **will the dependent crash, or start
broken, if this isn't there when it boots?** A database the connection pool
opens at startup, or an S3 bucket a service downloads its model from, is
required. A webhook receiver nobody calls until a user clicks something isn't.

## Hostnames are sugar, not edges

`${FGHJ_SERVICE_FQDN:billing/api}` expands to a hostname. It isn't an edge,
and doesn't make one: a service may as well hardcode
`https://api.billing.fghj.internal`, and fghj can't see that. Deriving edges
from templates would make the graph depend on how an address happened to be
spelled. The UI couldn't draw a dependency the user never declared, either.

A template still has to name an existing service, so that it can expand. Any
service in the workspace may be named, including another repo's, as
`alias/name`.

So **"uses" edges are gone.** The sidecar can still report a lookup of a
service that isn't running. That's a runtime observation, not part of the
graph.

## Tasks

A task is a service that is expected to exit. It is one when either:

- something requires it with `condition: service_completed_successfully`, or
- it declares `run: on_start` or `run: once`.

The second rule closes the old gap. Before, a seed listed only in a flow had
nothing waiting on it, so its clean exit read as a crash.

`condition: service_completed_successfully` together with `required: false`
is a blocking error. "Must have finished" is a start-time statement, so it
can't hold for a dependency the service doesn't need in order to start. A seed
only one journey needs has `run: on_start`, its own `depends_on` (on the
database it seeds), and is listed in that journey's flow.

Other conditions on a `required: false` edge are ignored, with an advisory.
Nothing waits on a runtime dependency.

## Waiting is per edge

Each node used to wait on its own healthcheck, whoever needed it. Now a node
is waited on only if something in the run requires it. A database that only
runtime dependents call no longer delays the rest of the run. Tasks are always
waited on, because their exit code is their result.

The health wait stays best-effort, as described in
[Concurrency model](/concepts/concurrency-model/): a healthcheck that never passes within the budget is
reported, and the dependents start anyway. A failed task, or a container that
can't be created, does fail, and its required dependents are blocked.

## Failure blocks dependents, not the run

A whole-run start used to stop at the first failed node. A named run also
rolled everything back. Now:

- A failed node's required dependents, and theirs in turn, are skipped. Each
  one gets the event "blocked: db failed".
- Everything else keeps starting. A broken worker doesn't stop an unrelated
  frontend.
- The run then reports an error naming what failed and what was blocked.
  Whatever came up stays up and visible, for named runs too.

Only failing to create the run's network or sidecar still rolls a named run
back, because nothing can start without them.

## Restart and stop follow required edges

- **Recreating a node** restarts the running services that require it, after
  it is ready again. This applies to a top-up recreating it for config drift,
  and to the Drawer's Start button. A service holding a pool against the old
  database would otherwise keep failing until someone noticed. A re-run task
  doesn't restart anything: `on_start` tasks re-run on every top-up, and
  bouncing the whole environment each time would make top-ups useless.
- **Starting one node** also starts the required dependencies it has that
  aren't running.
- **Stopping a node** stops the running services that require it, first.
  They'd fail anyway, and a stopped required dependency they don't know about
  is harder to read than a stopped service.


## Flows are orthogonal

Flows say *what to start*; edges say *how starting goes*. They meet in one
place: the required closure. A flow lists services, the run adds everything
those need to start, and `required: false` dependencies stay out unless a flow
lists them too. A flow never adds an edge, and an edge never adds a flow.
