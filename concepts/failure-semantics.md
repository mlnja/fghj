# Failure semantics: what is fatal, what is advisory, where it surfaces

## The problem

Every other concept file in this folder describes a mechanism working. This
one describes what happens when one doesn't — which is a policy question, and
for a long time it was answered differently in every file that faced it. One
check aborted a whole workspace; another pushed a string into a list nobody
read; a third rolled back three containers because a fourth failed, and a
fourth deliberately didn't.

The answers are not the same everywhere, and shouldn't be. But they should be
*chosen*, and this is where the choices are written down.

## The three questions

For any failure, three things have to be decided independently:

1. **Blast radius** — does this failure take down the thing it happened to,
   the workspace, or the daemon?
2. **Rollback** — does work already done get undone, or kept?
3. **Surface** — where does a human find out?

Most of the bad old behaviour came from answering one of these and assuming
the others followed.

## Blast radius: the unit of failure is the smallest independent thing

The governing rule comes from [[flat-workspace-model]]: repos in a workspace
are peers with no ownership relation, so one repo's broken file is not a
statement about any other repo.

`scan_workspace` therefore skips and reports a repo whose `.fghj.yaml` won't
parse, rather than propagating the error. Aborting the whole scan meant one
bad file anywhere 500'd the graph endpoint — so the UI could not render the
very workspace you needed to look at in order to *find* the bad file. The
`Result` that function still returns is reserved for the workspace directory
itself being unreadable, which is not a per-repo problem and leaves nothing
to partially resolve.

The distinction between *skipped* and *stubbed* is the subtle half. A missing
repo legitimately stubs: fghj genuinely doesn't know what it declares yet, and
a stub says so honestly. A malformed repo is present and says something fghj
cannot read — treating it as "declares nothing" would substitute a guess for
the author's intent, silently, and every node that depended on it would
resolve to a dangling reference with no explanation. So it becomes a blocking
warning instead: you can see the workspace, and starts refuse until it's
fixed.

## Severity: can the config be carried out as written?

`resolver::Severity` has two values, and the test that separates them is not
"how bad is this" but **whether the author's config can be honoured as
written**.

- **`Blocking`** — it can't. A dependency that doesn't resolve, two nodes
  claiming one name, a file that won't parse. Starting anyway produces a run
  that is quietly *not the one the config describes*, which is worse than not
  starting, because the user will debug the application instead of the config.
- **`Advisory`** — it can, and something is nonetheless pointless or
  surprising. A `wildcard` on a port with no domain to wildcard changes
  nothing about whether the run works. Report it; start anyway.

This replaced a flat `Vec<String>` whose only consumer was a UI banner.
Nothing on the start path read it — `start`, `ensure_running`,
`restart_container` and `perform_create` all take a `&Graph` and none of them
looked at `.warnings` — so a workspace with a dangling shared-backing
reference and two nodes claiming one domain started happily and misbehaved at
runtime, with the explanation sitting unread on a different screen. Making
every warning fatal would have been the wrong fix; giving the list a type
that the start path can *act* on was the right one.

## Rollback: the two start paths disagree on purpose

`start` (a named run) and `ensure_running` (the shared default
environment) do opposite things when a node fails partway through. This looks
like an inconsistency and is in fact the same principle applied to two
different situations.

`start` **rolls back**: it stops and removes every container it created, the
sidecar, and the network. A named run is a unit — it was requested as a
whole, nobody was using it a moment ago, and a half-built review environment
is not a useful artifact. Leaving three of five containers up would leave a
thing that looks like a run and isn't one.

`ensure_running` **keeps what came up**. It tops up the one shared default
environment, which someone is very likely working in right now. Tearing down
three containers that started fine because the fourth didn't would destroy
exactly the progress the user cared about — and it would do so in response to
a failure that may be entirely about the fourth node. Each node is reported
through `progress` as it comes up, and the error additionally carries the
whole partial state (`RunCreateError::partial`), so those containers stay
visible and routable rather than running unseen.

The rule underneath both: **roll back a thing that was requested as a unit;
keep a thing that was requested as an increment.**

Rolling back is still not an excuse to be violent about it. `start`'s
rollback calls the same `docker::stop_and_remove` as an ordinary teardown,
which means it stops the container and lets its own grace period run before
removing it — see [[docker-and-downloads]]. A rollback is triggered by a
*later* node failing; the containers being torn down have done nothing wrong,
and one of them may have already written to a `scope: stable` volume. "This
operation failed" is not a reason to SIGKILL a database.

There is a second reason the incremental path reports per node rather than
only at the end. A daemon that dies on node four must not lose the record of
nodes one through three — so `report(progress, …)` happens *before* the
container is folded into the local map, and `Action::RunCreateProgress`
reaches the reducer as each node comes up. Drain is ordered before settle so
a late progress report can't re-add a container the settle already dropped.

## Surface: the HTTP response is not where failures live

Every mutating endpoint follows a dispatch-and-return contract: the handler
dispatches an `Action` recording *intent* and returns immediately. The
reducer records the intent (`pending_action`, `pending_create`,
`pending_teardown`); `effects::docker::converge` does the real Docker work
afterwards and reports back with `*Settled`.

So a `200` from `POST /runs` means "the intent was accepted", never "it
worked". This is inherent to the architecture ([[state-and-effects]]) and not
a defect — a container start can take minutes — but it does mean the
response body cannot be the failure surface, and the ways a failure actually
reaches a human have to be enumerated:

- **Node events** (`record_event` → `GET .../events`) — the ArgoCD-style
  step-by-step narration of what `fghjd` is doing for one (run, node,
  action) cycle: resolving config, building, creating, waiting on health.
  A failed step is recorded with its detail. This is the primary surface for
  "why didn't my node start", and each new cycle clears the previous one for
  that action so the pane shows the current attempt, not a history.
- **Daemon logs** (`daemon_log` → `GET /daemon/logs`) — process-level
  operational output, for failures with no node to attach to.
- **Resolver warnings** — everything known before anything runs.
- **Container state** — a crashed container's `exit_code` and status, via
  the once-a-second observation pass.

`record_event` and `begin_event_cycle` are both **best-effort**: a failure to
write the narration logs to stderr and is otherwise ignored. Failing a
container start because its audit trail couldn't be written would turn a
cosmetic problem into an outage.

**One honest gap.** The error string in `ContainerActionSettled { result:
Err(msg) }` is used only to clear `pending_action`; the reducer drops the
message. So the *reason* a per-node action failed lives in the events table
and the daemon log, not in workspace state — which means a UI showing only
state sees a node that stopped being pending and never learns why. Nothing
is lost, but it is one click further away than it should be.

## The one guarantee that is structural

`SettleGuard` and `CreateSettleGuard` are RAII: they dispatch a settle action
on `Drop` unless a real outcome was already recorded. A panic unwinding
through an in-progress `.await` drops every live local in the async fn's
frame exactly as it would in a synchronous function, so a panicking or
cancelled converge task still reports — as an `Err` naming that exact case.

This matters more than it looks. Without it, a panic mid-action leaves
`pending_action` set forever, and the in-flight dedup check
(`ActionRejected::AlreadyInFlight`) then rejects every subsequent attempt on
that node for the life of the daemon. The failure mode of a missing report
is not a missing message; it is a permanently stuck node. Making the report
a destructor rather than a line at the end of the happy path is what makes
"always reports" true by construction.

## Related

- [[state-and-effects]] — why intent and outcome are separate actions.
- [[concurrency-model]] — what `AlreadyInFlight` protects.
- [[run-lifecycle-and-registry]] — the two start paths in full.
- [[config-language]] — where blocking warnings come from.
- [[flat-workspace-model]] — why per-repo isolation is the default.

## Status

Implemented: `src/resolver/warning.rs` (`Severity`, `Graph::blocking`),
`src/resolver/workspace_scan.rs` (per-repo isolation),
`src/runs/orchestrate.rs` (the two rollback policies),
`src/runs/events.rs` (best-effort narration),
`src/effects/docker/converge.rs` (`SettleGuard`, `CreateSettleGuard`),
`src/daemon_log.rs` (process-level output). Open: the settle error string is
not retained in workspace state.
