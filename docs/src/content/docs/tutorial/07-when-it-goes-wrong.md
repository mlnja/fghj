---
title: 7. When it goes wrong
description: The status vocabulary, configuration drift, blocking warnings, and the three places to look when something isn't up.
sidebar:
  order: 7
---

Everything worked in the first six chapters because it was written to. This
chapter is the one you'll come back to.

## A node's status is a pair, read as one word

fghj tracks two facts per container — what it was **asked** for and what
Docker **reports** — and derives a single word from the pair. That
derivation lives in exactly one place, so the UI, the CLI and the reconciler
can't disagree about what a state means.

| Status | What it means |
|---|---|
| **running** | Docker reports it up. |
| **stopped** | Docker reports it down, and fghj was not asked to keep it up. Expected, uninteresting — this is what you get after a Stop. |
| **crashed** | fghj *was* asked to keep this up and Docker says it is down. Nothing will restart it on its own — press Start. |
| **restarting** | Docker is bouncing it under this node's own `restart` policy. Not reachable while it does, but not stuck either — worth waiting out. |
| **paused** | `docker pause`, from outside fghj. Not reachable, and Start will not help; `docker unpause` will. |
| **completed** | A task exited 0. Success, not drift. |
| **failed** | A task exited non-zero. Everything downstream of it was not started. |
| **finishing** | A task has exited but fghj hasn't re-inspected it yet — no exit code read, so no verdict. |

The distinction that matters most is **crashed vs. stopped**. They look
identical to Docker — the container isn't running either way — and they need
opposite responses from you. One is the expected result of a button you
pressed; the other is your service dying, and staying dead, because
`restart` defaults to `"no"` and fghj deliberately doesn't second-guess that.

On top of that settled reading, a live action wins while it's in flight: a
node mid-`stopping` shows `stopping`, not the state it's about to leave. That
layering is why the settled word stays honest — it never has to encode
"…but something is happening".

## Drift: the container no longer matches the file

Edit `.fghj.yaml` while a run is up and nothing happens immediately. fghj
notices, says so, and waits for you:

| Verdict | Means |
|---|---|
| **synced** | The running container matches what `.fghj.yaml` would produce right now. |
| **drifted** | It doesn't. Shown as a `desired ≠ actual` pill: *the running container's config no longer matches .fghj.yaml — restart this node to pick up the change.* |
| **orphaned** | The container is running but its node is **no longer in the graph at all** — shown as `no longer declared`. |
| **unknown** | No drift check has run yet, or the last re-resolve failed. Nothing conclusive to say. |

fghj does nothing about drift on its own. That's a policy choice, not a
missing feature: the observation has to be representable before any policy
about it can exist, and the right response genuinely varies. Sometimes you
want the change picked up now; sometimes you're mid-debug on a container you
don't want restarted out from under you.

When you do want it applied, the node drawer's **Reset** button stops and
recreates that one container fresh — and leaves its volumes alone. Or press
**Start default environment** to top the whole run up, which recreates
exactly the nodes whose config changed and reports why:

```
recreating container: config changed since it was started
```

**orphaned** is worth understanding because its usual cause is innocent: you
ran `git switch` in a workspace checkout, and the branch you moved to
doesn't declare that service. fghj is a passive observer of branch state, so
the graph changed under a live run. The container keeps running, keeps its
route, and keeps its volumes; nothing reconciles it until you stop it or
switch back. Deleting a repo from the workspace does the same thing.

Drift detection sees a new commit, too — not just an edited `.fghj.yaml`. A
`git pull` that changes your build context is drift, because the image the
container is running is no longer the image that repo would build.

## Blocking warnings refuse the start

The resolver emits two kinds of finding. **Advisory** warnings are
observations you can ignore (a port with a `wildcard` that has no domain to
wildcard). **Blocking** ones mean the graph is not startable, and a start
attempt fails with all of them listed at once:

```
refusing to start: the workspace has 2 unresolved configuration problems:
```

All of them, not the first — they're usually independent mistakes, and
fixing them one round-trip at a time is miserable. The things that land here
are ambiguity and real contradictions: a dependency cycle, two nodes deriving
the same domain, a `kind: service` dependency on a repo with several services
and no `services:` list, a `shared-backing` pointing at a backing dependency
nobody declares.

This is why `fghj validate` is worth running while editing. It won't catch
these — they're whole-graph properties, not file-level ones — but it catches
the file-level half first, which is most typos.

## Three places to look

**1. The node drawer.** Click any node. Its kind, repo, branch and dirtiness;
its image; every port with both addresses to reach it on (the node's raw
domain, and the `127.0.0.1:port` Docker actually published it to — each one a
click to copy); its container name; its condition; its drift verdict; and its
container logs. Nine times in ten the answer is in the logs and the trip to
the terminal is unnecessary.

**2. `fghj exec`.** The container is up but behaving oddly:

```bash
fghj exec web.storefront -- sh
fghj exec web.storefront -- env
fghj exec web.storefront -- getent hosts db.web.storefront.shop.fghj.raw.internal
```

Those three answer, in order: what's in there, did my templates expand to
what I expected, and does in-network DNS agree. The second one is the reason
`exec` is the first thing to reach for after the logs — the container's own
environment is the only place the expanded templates are visible. A literal
`${FGHJ_SERVICE_FQDN:...}` still sitting in that output means the name didn't
match any sibling: usually a typo, or a node you didn't actually declare a
dependency on.

**3. The daemon.** The ⚡ button opens `fghjd`'s own telemetry: its log, plus
DNS and port-forwarding status. Start from the CLI:

```bash
fghj daemon status
```

## The specific things that go wrong

**A domain doesn't resolve at all.** Check `fghj daemon status` first. If
`fghjd` is running but idle it has released ports 80/443 and DNS;
`fghj daemon start` reconciles it back to active. On macOS the split-DNS hook
is `/etc/resolver/fghj.internal` — if that file is gone, `fghj daemon
restart` rewrites it.

**The browser shows a certificate warning.** The local CA isn't trusted.
`fghjd` verifies and re-installs its own CA into the system keychain on every
start, so a restart usually fixes it. It only ever writes when trust is
genuinely absent, which is why it can do that check every time.

**A name resolves but you get fghj's 404 page.** DNS and TLS both worked and
there's no route registered for that name — so the node isn't running, or
isn't the one you think. Check the node's status, and check whether you're
using a run-scoped name for a node in a different run.

**`fghjd` won't start.** It needs root (ports 80/443, DNS, the trust store,
`/etc/hosts`) and it needs to reach Docker. It refuses to start rather than
half-work if Docker isn't there.

**A task went red and nothing downstream came up.** That's `failed` doing
its job: a non-zero migration blocks its dependents rather than letting them
start against a half-migrated database. Read the task's logs in the drawer,
fix the cause, and start again — `on_start` tasks re-run on every top-up.

**Everything is slow the very first time.** The first run of a workspace
pulls the sidecar image, then builds your service images. Later runs reuse
both.

---

## Where to go from here

You've used most of the language. What's left is breadth, not new concepts:

- [.fghj.yaml reference](/reference/fghj-yaml/) — every field, including the
  ones this tutorial skipped: `env_file`, `additional_hosts`, `extra_hosts`,
  `shared-backing`, `platform`, build secrets and ssh-agent forwarding,
  bind-mounting your source for live reload.
- [HTTP vs. raw: choosing a zone](/guides/networking-http-vs-raw/) — the long
  version of chapter 2's table.
- [Concepts](/concepts/architecture/) — one page per subsystem, each
  explaining why it is the way it is. Start with
  [Node identity & domains](/concepts/node-identity-and-domains/) if you
  liked the id-derivation parts of this tutorial.
