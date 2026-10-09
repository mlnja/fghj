# Flows v2: Compose-shaped services, flows as public start lists

Status: **implemented** (config `2.0`; the 1.x format and its migrator have
since been removed). Replaces the `kind:`-based dependency list and the 1.x
`flows:` block. Needs config format `2.0`.

## The problem

Two big repos, A (`shop`) and B (`billing`), each with ~10 dependencies. One
user journey in A needs B, but only the part of B that uses one of those ten.
Starting everything works but is slow and heavy. Starting less needs someone
to say *what* "less" is, and every place to write that down looked wrong:

- In A's file: A has to know B's internals, and goes stale whenever B changes.
- In B's file: B can't know every journey every consumer has.

A second, separate complaint: the config doesn't read like Docker Compose.
Postgres isn't a service, it's a `kind: backing` entry buried in another
service's dependency list.

## Can this be solved at all?

**Not exactly.** "Which services does journey J need" is a property of what
the code *does* at runtime: which branches run, which feature flags are on,
which input the developer types. No config, and no static analysis, can
compute it exactly. Any answer is an approximation, and it can be wrong in two
directions:

- **Too much** (over-inclusion): wasted memory and start time. Safe.
- **Too little** (under-inclusion): the journey breaks. The failure that
  matters.

So the best achievable design is not "the exact set". It is:

1. **Safe by default.** With no information, start everything. That is what
   production does, so it is never wrong.
2. **Trimmed by whoever actually knows.** Each fact written once, by the team
   that can know it, next to the code it describes.
3. **Misses detected at runtime, fast, with the fix pointed at the right
   owner.** The in-network sidecar already sees every DNS lookup; a lookup of
   a node that is declared but not started *is* an under-inclusion, caught
   the moment it happens (see [[in-network-sidecar]]).

This design is that, and the argument that it's the best config shape is the
ownership table below: no fact is written by someone who can't know it, and
no fact is written twice.

| Fact | Who can know it | Where it's written |
|---|---|---|
| What B's api can't start without | B | B: `depends_on` |
| What B's api uses only sometimes | B | B: `depends_on: {x: {required: false}}` |
| Which of B's parts a B journey needs | B | B: `flows:` |
| That A's journey uses B's journey | A | A: `flows:` |
| That A can't start until B's journey is up | A | A: `depends_on` on B's *flow* |
| B's network address | B publishes, A references | A: `${FGHJ_SERVICE_FQDN:billing/api}` |

Every alternative considered (see the last section) moves at least one row
to someone who can't know it, or writes a row twice.

## The config

### B (billing)

```yaml
version: "2.0"
services:
  api:
    build: .
    depends_on:
      postgres: {condition: service_healthy}   # can't start without it
      migrate:  {condition: service_completed_successfully}
      search:   {required: false}              # used in some journeys
      queue:    {required: false}
  postgres:
    image: postgres:16
    healthcheck: {test: [CMD, pg_isready]}
  migrate:
    build: .
    command: [rake, db:migrate]
    depends_on: [postgres]
  search: {image: opensearch}
  queue:  {image: rabbitmq}

flows:
  pricing: [api, search]
  reindex: [api, queue]
  db:      [postgres]
```

### A (shop)

```yaml
version: "2.0"
include:
  billing: git@github.com:org/billing.git      # alias defaults to the repo name

services:
  web:
    build: .
    depends_on: [redis]
    environment:
      BILLING_URL: https://${FGHJ_SERVICE_FQDN:billing/api}
  redis: {image: redis:7}
  migrate:
    build: .
    command: [rake, db:migrate]
    depends_on:
      billing/db: {condition: service_healthy}   # a flow, not a service
    environment:
      DATABASE_URL: postgres://${FGHJ_SERVICE_FQDN:billing/postgres}/shop

flows:
  checkout: [web, migrate, billing/pricing]
```

Running `checkout` starts web, redis, migrate, and from B: api, search,
postgres, migrate (B's). B's queue and everything else in B stays stopped.

## The rules

1. **Everything is a service.** An entry with `build:` is your code; with
   only `image:` it's a backing service (postgres, redis). There is no
   `kind:`. A service is a **task** (expected to exit, exit 0 = success) when
   anything waits on it with `condition: service_completed_successfully`, or
   when it declares `run:`.
   Waiting on the same service with both "completed" and "healthy"/"started"
   is a blocking error.
2. **`depends_on` describes production.** Compose syntax: a short list, or
   the long form with `condition` and `required`. An entry is "needed to
   start" unless marked `required: false`, which means "needed at runtime".
   What each kind does is in [[dependency-kinds]].
3. **Within a repo, `depends_on` names services. Across repos, it names
   flows** (`billing/db`). Never another repo's services. Waiting on a flow
   means waiting until every member of it meets the condition, and it adds
   that flow to the run.
4. **`include:` is the link to another repo.** It means: clone it, and when
   starting everything, start all of it.
5. **A flow is a list.** Each entry is one of your own services, another
   repo's flow (`billing/pricing`), or another repo as a whole (`billing`).
   Flow names are scoped to their repo: A's `checkout` and C's `checkout` are
   different flows.
6. **What a run starts:** the members of its flows, expanded recursively,
   plus everything those members can't start without (rule 2, followed within
   each repo, and across repos via rule 3). Each `(repo, flow)` is expanded
   once, so references between flows can be circular.
7. **No flow = everything.** Every service in this repo and in every repo it
   includes, all the way down. Starting one service from the UI is the same
   as a one-entry flow.
8. **Hostnames are the only exception.** `${FGHJ_SERVICE_FQDN:billing/api}`
   may name another repo's service, because the address is the network
   contract: production has it in A's config too. A hostname is sugar, not an
   edge: it never orders, waits or draws a line (see case 5). A service may
   just as well hardcode the address.
9. **Start order.** Within a run, every required `depends_on` is respected.
   `required: false` never orders anything. A cycle of required edges is a
   blocking error: it can never start.

What goes away: `kind: service | backing | shared-backing | task`, `after:`,
per-edge `repo`/`services`/`default_branch` (moves into `include:`), and the
`description`/`service`/`dependencies` shape of today's flows.

## Cases

Each one followed through the rules above. ✅ works as is, ⚠️ works with
the check or tool named, ❌ doesn't work.

**1. The original case.** A's `checkout` uses `billing/pricing`. A never
names anything in B except flows and hostnames. B's other dependencies stay
stopped. ✅

**2. No flow.** Everything in A, B, and whatever B includes. Same as
production. ✅

**3. A needs at runtime something this run doesn't start.** Suppose web
declares `billing/webhooks: {required: false}`, and the run's flows don't
include it. ⚠️ Advisory at start, not blocking: "web needs billing/webhooks
at runtime, but this run doesn't start it." Blocking would make it impossible
to start a flow on purpose without a service you know you won't call. A
hostname alone says nothing: fghj can't see a hardcoded one, so it doesn't
read templates either.

**4. Shared database.** A's migrate writes to B's postgres and must not run
before it is up. Rule 3: A waits on B's flow `db`, which B publishes. Without
cross-repo waiting, migrate would run first, fail, and block web. ✅

**5. Services that call each other.** A calls B; B sends webhooks to A. Each
declares the other `required: false`, so neither waits, and there's no cycle.
If both said `required: true`, that is a real cycle and a blocking error:
neither could ever start. ✅

**6. A waits on B's flow, B waits on A's flow.** A start-order cycle across
repos. It can never start. Blocking error naming both edges. ✅

**7. Monorepo.** api, worker and admin share one postgres. `jobs: [worker]`
brings postgres along because worker can't start without it. Sharing needs no
config; there is no `shared-backing`. ✅

**8. Tasks.** B's migrate is brought in automatically because api waits for
it to finish. A seed only one journey needs declares `run: on_start` and is
listed in that journey's flow. Nothing waits on it, but `run:` says it's a
task, so a clean exit reads as success. ✅

**9. B marks a needed dependency optional.** api starts without postgres
and looks up its hostname during startup. The sidecar sees the lookup and
points at B: "api looked up postgres while starting; it shouldn't be
`required: false`". ⚠️ Sidecar check.

**10. Drift: `pricing` now also needs queue.** If B's developers hit it
first (they run `pricing` daily), they fix B's flow and A picks it up with no
change. If A hits it first, the sidecar offers "Start queue" for the running
run. A can't fix B's file, and shouldn't. ⚠️ Needs a local, per-machine
override ("billing/pricing += queue") that removes itself once B's file
contains it.

**11. B's checkout is on a branch without `pricing`.** Blocking error:
"billing (branch `release-3`) has no flow pricing; it has: reindex, db". The
checkout on disk really doesn't have it, so the error is correct. The
upside: B's flows always match B's code on that branch, because they're in
the same commit. ✅

**12. B publishes no flows.** A can only write `billing`, which is all of
it. Nothing is trimmed until B publishes flows. The pressure lands on the
team that can act on it. ✅ by design.

**13. A needs two of B's flows.** `[web, billing/pricing, billing/reindex]`:
one api, with search and queue. ✅

**14. Flows referencing each other across repos.** `checkout` uses
`billing/pricing`, which uses `shop/catalog`. Each `(repo, flow)` is expanded
once. Repos including each other are cloned with a visited set. ✅

**15. `billing` includes C.** "All of billing" means all of C too. That's
the production meaning, but it can be large. ⚠️ The UI shows how much a
run will start before starting it.

**16. Adding a flow to a running run.** You're running `checkout` and add
`billing/reindex`. queue starts. api doesn't restart: names are deterministic,
so its environment already had queue's hostname, which now starts resolving.
✅

**17. Runtime dependency ordering.** `pricing: [api, search]`. search is
`required: false` for api, so the two start in no particular order, even
though both are in the run. api doesn't need search to boot. In `reindex`,
search isn't there, and the start says so. ✅

**18. Same flow name in two repos.** Scoped by repo (rule 5). Today they
merge. ✅

**19. Two of B's flows define the same service differently.** Can't happen
any more: a service is defined once, in `services:`. Today two flows can
declare the same backing name on one root, and the first silently wins. ✅

**20. Graph view.** Every line is a declared `depends_on`: solid for needed
to start, dashed for needed at runtime. Nothing is inferred, so the picture
is exactly the config. ✅

**21. A teammate on fghj 1.x.** A `2.0` file is refused by 1.x with a
version error, never misread, and fghj 2.x refuses a 1.x file the same
way. ✅

**22. A wants a slice of B that B didn't publish.** E.g. api with search but
also B's webhook receiver, which is only in `reindex` along with queue. A
takes `pricing` + `reindex` and gets queue too: over-inclusion, which is
safe, or asks B to publish a finer flow. ⚠️ See "the one real trade-off".

## The one real trade-off: encapsulation vs precision

The finer B's flows, the more precisely A can trim. But a flow per optional
dependency is just B's internals with extra steps. The coarser B's flows,
the better B is encapsulated, and the more A over-starts.

This can't be designed away, only placed. Here it's placed with B: B decides
how much to publish, the default (no flows) is safe, and the cost of B
publishing too little is over-inclusion (slow), never breakage. That's the
right direction for the cost to fall.

## On-demand start: does it make flows unnecessary?

The sidecar sees every lookup of a stopped node. It could *start* the node on
that lookup and hold the answer until it's healthy, like socket activation.
Then nobody would write flows at all.

It doesn't replace them:

- **Timeouts.** DNS clients give up after a few seconds. postgres takes
  seconds to become healthy, opensearch tens of seconds. Holding the answer
  means the sidecar must also proxy the connection (every protocol, not just
  HTTP) and keep it open until the node is up, and many clients time out
  anyway.
- **Startup connections.** Many apps open their connection pool once, at
  boot. If it fails, they crash, and on-demand start fires too late.
- **Ordering.** Tasks (migrations) must run before their dependents. A
  lookup-triggered start has no idea a migration should have run first.
- **Non-determinism.** What starts depends on which code paths happened to
  run. Two developers running the same journey get different sets.

What's worth keeping from it: an opt-in "start on miss" in the UI, and a
recorder ("run everything, watch which lookups happen during this journey,
suggest a flow"). Both are tools for *writing* flows, not substitutes. The
recorder must attribute what it learns to the right repo: a lookup made by
B's api becomes a suggestion for B's flow, never a list of B's services in
A's file.

## Alternatives rejected

- **A lists B's services** (today's model, and `kind: service` with
  `services:`). A writes B's facts: goes stale on every B change and every B
  branch.
- **Optional dependencies plus "include this optional dep" from A.** Same
  problem: A names B's internals.
- **Compose `profiles` (tags on nodes, activated per run).** Makes the graph
  conditional, so the file no longer describes production. Needs tags on
  edges for cross-repo use, plus a second field to activate the other repo's
  profiles. Much more to learn for the same result.
- **Capabilities/tags B exposes.** That's flows under another name; a flow
  list is the simpler form.
- **Flows anchored to a root service** (`service:` + additive
  `dependencies:`). Mixes "what to start" with "add edges to the graph", and
  the added edges leak into every other run. A list adds nothing to the
  graph.
- **A central workspace file.** No team owns it, and [[flat-workspace-model]]
  has no root repo by design.
- **Hostname references imply waiting, or are edges at all.** Waiting
  deadlocks services that call each other (case 5). Drawing them makes the
  graph depend on how an address was spelled.

## Decisions taken

- **A task that nothing waits on** (case 8): declare `run:`. That alone
  makes it a task.
- **The default `condition`**: fghj's, not Compose's. A required edge waits
  for its target to be ready: healthy if the target declares a
  healthcheck, exited 0 if it's a task, started otherwise. `condition` is
  checked against the target (waiting on a task with anything but
  `service_completed_successfully` is blocking), but never weakens the wait.
  A target only runtime dependents need isn't waited on at all. See
  [[dependency-kinds]].
- **Hostnames aren't edges.** An earlier version drew a dashed "uses" edge
  for every template reference. That's gone: the graph shows what's
  declared, and a hardcoded hostname would have been invisible anyway.
- **Separator**: `/`. `.` reads like a domain, and fghj domains run the
  other way.
- **A bare alias in `depends_on`** waits on that repo's own services only,
  not on what it includes in turn: those are already waited on by its
  services, and an include pointing back would make a repo wait on itself.
  A bare alias in a *flow* stays transitive — "all of it", as in
  production.

## What changes in fghj

1. `schema/*.cue`: the `2.0` format above; refuse mixing 1.x and 2.0.
2. A one-time 1.x → 2.0 migrator (since removed, with the 1.x format). Node ids change (`postgres.api.billing`
   becomes `postgres.billing`), so named volumes keyed by the old id would
   come up empty. The migrator lists each one with its old and new key
   rather than renaming Docker volumes behind the user's back. Run in the
   workspace folder, it also turns cross-repo `shared-backing` into a
   published `shared` flow, so the dependent waits on exactly what it used.
3. Resolver: services keyed by `(repo, name)`, flows by `(repo, flow)`; the
   run's set computed by rules 6–7, forward only. Today's flow membership
   walks edges in both directions and effectively selects everything
   connected, so this is a fix even before anything else ships.
4. Start order and cycle checks over the run's own set (rule 9).
5. Start-time checks: `required: false` dependencies not in the run
   (case 3); conflicting task conditions (rule 1).
6. Sidecar: the list of declared-but-not-started names, misses reported and
   attributed to the owning repo (cases 9–10); local overrides (case 10).
