# Debugging in containers

## The problem

A debugger is the one tool a developer reaches for that a container actively
gets in the way of. The process is on the other side of a network namespace,
its debug port is not published, the IDE has no address to dial, and the
language runtime usually has to be *told at launch* to listen at all — so by
the time you want a debugger you need to have already asked for one.

The easy answer is also the wrong one: fghj could learn each runtime's
activation incantation (`NODE_OPTIONS=--inspect`,
`JAVA_TOOL_OPTIONS=-agentlib:jdwp=...`, `RUBYOPT=-rdebug/open`,
`dlv exec --headless`) and inject whichever one matches the image. That is an
adapter table, and an adapter table is a permanent liability: it is wrong for
every image that wraps its entrypoint in a shell script, it is wrong for
`gunicorn`, it is wrong for the next runtime, and it is wrong the moment
someone's `Dockerfile` already sets `NODE_OPTIONS` for a reason of its own.
Worse, it makes fghj responsible for guessing what is inside an image it did
not build.

## The shape: fghj owns the address, the image owns the debugger

The whole feature is one declaration and three environment variables.

```yaml
services:
  api:
    debug: 9229
```

`debug: <port>` does exactly one thing: it **publishes that port** like any
other declared port, so the debugger has a stable, printable address.

It injects nothing into the container's environment.

One variable exists in the whole feature, and `debug:` is not what sets it:

| Variable | When | Value |
|---|---|---|
| `FGHJ_DEBUG_WAIT` | only when the UI switch is on for this container | `1` |

That is the entire contract, and fghj knows nothing beyond it. It does not
know which debugger this is, which runtime the image runs, or whether
anything is listening. An image that never listens makes `debug:` inert,
which is a harmless no-op. Nothing is probed, nothing is guessed, and there
is no per-runtime code anywhere in the daemon — the recipes live in
`guides/debugging.md`, which is documentation, not behaviour.

### Why no variable carries the port

An earlier draft of this design set two variables whenever `debug:` was
declared: `FGHJ_DEBUG=1` and `FGHJ_DEBUG_PORT=<port>`. Both were cut, in
that order, and the reasoning is worth keeping because it generalises.

`FGHJ_DEBUG=1` went first. It looks like a feature flag, but it never varied
*within* fghj: `debug:` lives in a committed file, so the flag was on for
every container of that node, always. The only question it could really
answer was "am I running outside fghj at all" — and the port variable
already answered that, so a shim had two ways to ask the same thing and
could be written to make them disagree.

`FGHJ_DEBUG_PORT` went second, and that is the sharper cut. **`debug:` is a
port declaration, exactly like `ports:` — and fghj injects nothing into the
environment for those either.** An application already knows which port it
listens on; `ports:` exists so fghj can publish and address it, not to tell
the app something it has to have decided already. A debug port is no
different: the number is written in the `Dockerfile` that starts the
debugger, and `FGHJ_DEBUG_PORT` would restate it in a second place, free to
drift out of agreement with the first. Anyone who *wants* a variable can set
one with `environment:` — it is theirs to name and theirs to keep correct.

What survives of `debug:`, then, over simply adding the number to `ports:`,
is *meaning*: fghj knows which of the node's ports is the debugger, so the
drawer can label that address and offer the halt switch for that node.

### Why "off" means *absent*, not `0`

When the UI switch is off, `FGHJ_DEBUG_WAIT` is not set at all. This is
deliberate: it makes the dumbest possible test correct in every language a
shim might be written in.

```sh
[ -n "$FGHJ_DEBUG_WAIT" ] && suspend=y
```

```python
if os.environ.get("FGHJ_DEBUG_WAIT"):
    debugpy.wait_for_client()
```

A `0` would be truthy in both of those. Someone would write that shim, it
would work, and then it would halt every container of that node, before line
0, waiting for a human who isn't coming.

### fghj never touches your Dockerfile

The image is the user's. fghj does not derive a second image from it, inject
an entrypoint wrapper, mount a shim binary, or inspect what the entrypoint
is. Every one of those was considered and dropped for the same reason: they
all mean fghj quietly produces a *different* container than the one the
`Dockerfile` describes, and when a debugger then fails to attach there is no
way for the user to tell whose fault it is.

So the division is: the docs tell you what to write in your own
`Dockerfile`, and fghj guarantees the contract and the address. Four lines
of shell in a file you control, versus a table of runtime knowledge in a
daemon you don't.

## The address comes free from raw ports

`debug:` is appended to the node's `port_list` in `resolve_node_spec`, and
everything else follows from machinery that already exists
([[two-zones-and-raw-ports]]):

- `docker.rs` publishes every entry of `port_list` to `127.0.0.1:<ephemeral>`.
- `state::query::raw_endpoints` reads the *full* observed port map — every
  published port, not only the routed ones — and feeds `raw_net::reconcile`.
- So the debugger answers at **`{node's raw domain}:{declared port}`**, on
  the number you declared, from the host *and* from inside the run's
  network, without colliding with the same port on another node or in a
  parallel run.

That last property is why the port is not published on the host under its
own number. Two services both debugging on 9229, or the same service in two
concurrent runs, would collide instantly — and the collision would be
silent, because whichever bound first would answer.

The port is published **unconditionally**, even with nothing listening on
the other side. That keeps the address stable and printable before anyone
attaches, and it means flipping the wait switch changes only the
environment, never the port set — so it cannot invalidate anything that
already recorded the address.

One exception: if the same number is already in `ports:`, the explicit
`#Port` wins and nothing is appended. That declaration may carry a pinned
`host_port` or a `name` (and so a nested domain) that a blind append would
clobber, and Docker would reject the duplicate binding anyway.

### The bug this ordering was hiding

`start_node` used to build the container's observed port map from
`node.ports`. With `debug:`, the port was published by Docker and then never
recorded — so `raw_endpoints` never saw it, `raw_net` never NATed it, and
the debugger was reachable at the ephemeral localhost port and nowhere else.
It now keys off `spec.port_list`, the set actually published.

## Halting at startup is the one thing that needs a switch

Listening is not waiting, and the distinction is the whole reason there is a
switch at all.

| Listening | Waiting |
|---|---|
| `node --inspect` | `node --inspect-brk` |
| JDWP `suspend=n` | JDWP `suspend=y` |
| `debugpy.listen()` | `debugpy.wait_for_client()` |
| `dlv --continue` | `dlv` without `--continue` |

A *listening* debugger costs essentially nothing and cannot stop anything,
because a breakpoint does not exist until an IDE has attached and sent it.
So the recipes listen unconditionally — there is nothing to toggle, which is
the other half of why no variable needs to carry the port.

Halting is different, and it is the thing you actually need for the case
that matters: a breakpoint on the first line of `main`. Your process will
always reach line 0 before an IDE can connect and install breakpoints, so
the only way to catch it is to not start. That is what `--inspect-brk` does
— the process blocks before user code, the IDE connects, fetches scripts,
sends `Debugger.setBreakpointByUrl`, then sends
`Runtime.runIfWaitingForDebugger`, and only *then* does execution begin,
with breakpoints already installed. JDWP does the same with `suspend=y`
followed by `VM_Resume`; debugpy blocks until the DAP `configurationDone`.

So `FGHJ_DEBUG_WAIT=1` is a switch, it defaults to **off** for every
container, and flipping it recreates the container to apply it.

### Why it is not in `.fghj.yaml`

`.fghj.yaml` is committed and shared. "Halt this node before line 0 and wait
for a human" pinned in that file would block *every* teammate's start of
that node, indefinitely, and stall everything downstream of it. It is not a
property of the node; it is something one person is doing to one container
for the next ten minutes. So it lives on the container
(`state::ContainerDesired::debug_wait`), is flipped from the UI per
container, and is persisted only so that an `fghjd` restart doesn't silently
drop someone out of a session.

### Why it is deliberately not in `spec_hash`

This is the load-bearing decision of the whole feature.

`ensure_running` — the top-up — **does** act on drift
([[config-drift]], `top_up_may_skip`). If `debug_wait` were part of
`spec_hash`, then the moment anyone pressed "Run flow", the halted container
would read as drifted and be recreated, destroying the debug session the
switch had just set up. And `config_drift` would report the node as out of
sync with a `.fghj.yaml` that never mentioned any of this.

So `start_node` computes `config_hash = spec_hash(node, &spec)` **first**,
and only then derives the container's actual environment from the spec via
`debug_wait_overrides`.

`debug:` itself still reaches the hash, and it does so purely through
`NodeSpec.port_list`: adding it changes the container's published ports,
which is genuine drift and should read as such. That is what
`adding_debug_to_the_config_reads_as_drift` pins, and it is why cutting the
environment variables cost the design nothing — the port set was always the
real carrier. `FGHJ_DEBUG_WAIT` is applied after the hash and is not in it.

The cost of that choice, stated plainly: a recreate for *genuine* config
drift drops the flag. That is why `debug_wait` is recorded on the container
rather than held in a side table — the switch the UI shows is then always
what the running container actually has, so it honestly flips off instead of
claiming a session that no longer exists. A top-up for drift
([[run-lifecycle-and-registry]]) does preserve it, since it has the previous
container in hand.

### The healthcheck has to go with it

A process halted before line 0 can never report Docker-healthy. Leaving the
healthcheck in place would make `wait_for_healthy` burn the node's entire
`PER_NODE_LIMIT` and stall every dependent behind a container that is
waiting for a human. So `debug_wait_overrides` drops the healthcheck too.

That is also the honest reading rather than a workaround: a halted process
genuinely has nothing to say about its own health, so fghj stops asking
instead of recording a failure.

## No new effect path

The switch is a value in `desired`, and `PendingAction::Starting` is already
the convergence that makes a container match its desired state. So
`Action::RunNodeDebugWaitRequested` records the new desire, sets
`desired.running = true`, and queues `Starting`; `effects::docker::converge`
reads `desired.debug_wait` back out when it calls `restart_container`. There
is no dedicated pending action and no new effect plumbing
([[state-and-effects]]).

It requires the container to already exist, unlike `start_node`, which
inserts a placeholder for a node with no live entry. A node with no
container has nothing to halt, and conjuring one would fold two intents —
"start this" and "halt it before line 0" — into one switch.

## What this does not defend against

A debug port is arbitrary code execution: an attached debugger can evaluate
expressions, call functions, and read every secret in the process.

In fghj it is local-only by construction, not by policy. Published ports are
bound to `127.0.0.1` only, and the raw-zone addresses are `10.222.x`
loopback aliases, which RFC 1122 forbids from leaving the machine. There is
no configuration that exposes a debug port to a network, because there is no
configuration at all beyond the port number. See [[security-model]] for the
rest of the threat model this sits inside.

## Related

- [[two-zones-and-raw-ports]] — where `{raw_domain}:{port}` comes from, and
  why the debug port needed no new networking at all.
- [[config-drift]] — the hash, and `ensure_running` as the one path that
  acts on a drift verdict.
- [[state-and-effects]] — why the switch is a desired value plus an existing
  convergence rather than a new effect.
- [[run-lifecycle-and-registry]] — `start_node`, `restart_container`, and
  the top-up that preserves the flag.
- [[config-language]] — `#RunOptions.debug` as an authoring aid, with the
  Rust types as the enforcing boundary.
- [[security-model]] — what a debug port grants, and why loopback is the
  whole defence.

## Status

Implemented: `schema/dependency.cue` (`#RunOptions.debug`),
`src/resolver/graph.rs` (`Node::debug`) and the three config structs that
feed it, `src/runs/node_spec.rs` (the published port and the two env vars),
`src/runs/start_node.rs` (`StartContext::debug_wait`,
`debug_wait_overrides`, applied after `spec_hash`),
`src/state/container.rs` (`ContainerDesired::debug_wait`),
`src/persistence/sqlite/` (the `debug_wait` column and its round trip),
`src/action.rs` + `src/reducer/run.rs` (`RunNodeDebugWaitRequested`),
`src/web/api/runs.rs` (`POST /runs/{run_id}/nodes/{node_id}/debug-wait`),
and the drawer's debug row + halt-at-startup switch
(`ui/src/lib/Drawer.svelte`, `ui/src/App.svelte`'s `setDebugWait`, which
goes through the same one-at-a-time action queue as Start/Stop because it is
a lifecycle action). `fghj graph` needs nothing: it prints the serialized
graph, so `Node::debug` is already in it.

There is deliberately no "waiting for debugger" `NodeCondition`. Dropping
the healthcheck means Docker reports the container as plainly `running`
rather than unhealthy, so there is no wrong verdict to correct — and the
switch in the drawer, read off `desired.debug_wait`, already says what is
true.

Per-runtime activation recipes are documentation only
(`guides/debugging.md` for all five, `guides/python.md` and `guides/go.md`
for the two worked out in depth, `tutorial/08-attaching-a-debugger.md` for
both halves end to end) and will stay that way — see the opening section for
why an adapter table in the daemon is not an acceptable shape.
