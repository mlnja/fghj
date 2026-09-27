# The boundaries of the language

## Why a boundary needs writing down

`.fghj.yaml` does not do everything Compose does, and it never will. Some of
what's missing is simply not built yet; some of it is *chosen*. Those two
categories look identical from the outside — both are "you can't express
that" — and the difference matters enormously to someone deciding whether to
adopt the tool or whether to send a patch.

So this file states the line. Everything on the "out of scope" side has a
reason. Everything on the "not built yet" side has no reason beyond nobody
having done it.

## The one real domain restriction: no host processes

fghj will not grow a node kind that runs a process directly on the host
instead of in a container. This is the only substantive restriction on what
the language can describe, and it was decided against a concrete case rather
than in the abstract.

The case: an adopting repo's Vite dev server ran as a bare host process bound
to port 80 — the same port fghj's own reverse proxy needs. A host-process node
kind would not have fixed that. The port collision is unchanged by who
launches the process, so the feature would paper over the symptom while
leaving the conflict.

The deeper reason is that everything the tool actually *does* is a Docker
concept. Routing is a published port plus a network alias. Domains are DNS
answers pointing at a proxy that dispatches to a container. Healthchecks are
Docker healthchecks. Volumes are Docker volumes. Log capture is a Docker log
stream. A host-process node kind would need a complete parallel
implementation of every one of those — a second lifecycle, a second logging
path, a second routing path, a second notion of "healthy" — inside a tool
whose entire value is that there is exactly one of each.

The alternative was available and better: containerize the dev server on the
adopting repo's side, as an ordinary `#Service` with a Dockerfile. That was
genuinely blocking before `#Volume` bind mounts existed, because a dev server
in a container without live bind mounts loses hot reload, which is the whole
point of a dev server. With bind mounts it is a normal service. The
restriction stopped costing anything the moment the feature it depended on
landed.

**The general form of the argument**, worth reusing: a capability that would
require a second implementation of the tool's core mechanisms is not a
feature, it is a fork. Prefer moving the work to the side that already has
the abstraction — here, the repo's own Dockerfile.

## The other boundary: fghj does not own your branches

[[branch-ownership-model]] states it in full, but it belongs on this list
because it is a restriction on the *language*, not just on behaviour: there
is no way to say "this dependency must be on branch X." Branch identity lives
on the one shared workspace checkout, and fghj is a passive observer of it.

The reason is the diamond: if a flow edge could carry a branch, two paths
through the graph could require two different branches of the same repo at
once, and there is one checkout. Rather than resolve that conflict (badly,
and at a point where the user isn't looking), the language declines to
express it.

## What is missing but not out of scope

Everything in the audit's K1 list is a gap, not a decision. Reproduced here
so the boundary file doesn't read as though these were chosen:

| capability | what it blocks |
|---|---|
| `entrypoint` override | only `CMD` is overridable; changing `ENTRYPOINT` means editing the Dockerfile |
| cpu / memory limits | "this environment fits in 8 GB" is unsayable; a JVM service can OOM the laptop |
| `tmpfs`, `ulimits`, `sysctls`, `shm_size` | `shm_size` in particular blocks stock Chrome and Postgres images |
| `devices`, GPU | any ML/CUDA environment |
| `stop_signal`, `stop_grace_period` | teardown is unconditional `force(true)` — no graceful shutdown, so a database can be SIGKILLed mid-write |
| UDP ports | `ports` keys match `^[0-9]+$`, so TCP only — no DNS-based apps, statsd, syslog, QUIC, game or VoIP servers |
| external named volumes | every volume name is derived; there is no way to reference a volume that already exists |
| the long tail | `security_opt`, `read_only`, `init`, pid/ipc/uts mode, `logging`, `dns_search`, `pull_policy`, `hostname`, `stdin_open`/`tty` |

None of these need a design decision. They need the six-step addition in
[[config-language]] and a minor version bump. Two are worth singling out as
more than mechanical: `stop_grace_period` because the current behaviour can
lose data, and UDP because the `^[0-9]+$` key pattern is a schema change
rather than a new field.

## How to tell which side a new request is on

The question is not "is this hard" or "is this Compose-compatible". It is:

**Does honouring this require a second implementation of something fghj
already has exactly one of?**

If yes — a second lifecycle, a second routing path, a second source of truth
for branch state — it is a boundary, and the answer is to move the work to
the side that already has the abstraction. If no, it is a knob, and it goes
through [[config-language]]'s addition process.

## Related

- [[config-language]] — how to add something that *is* in scope.
- [[branch-ownership-model]] — the other boundary, argued in full.
- [[flat-workspace-model]] — why the language has no notion of a root repo.
- [[docker-and-downloads]] — the bind mounts that made the no-host-process
  decision costless.

## Status

Decisions recorded: no host-process node kind (2026-09-07), no branch
declarations on edges. The K1 gaps above are open and mechanical; see
`concepts/AUDIT.md` K1 and `PROGRESS.md`'s "eventually" list for their
current state.
