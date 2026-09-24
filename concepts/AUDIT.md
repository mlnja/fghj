# Audit: expressiveness and soundness of the fghj model

A formal review of `concepts/` read against `schema/*.cue` and `src/`, answering
one question: **is the capability surface and the configuration language enough
to describe every system someone would want to run as a local dev env?**

This is not a concept doc — it does not settle a design decision. It is a
working record of where the model is short, with stable ids so findings can be
addressed one at a time. When a finding is closed, mark it in the tracker and
fold the resulting decision into the relevant concept file (or a new one); the
concept files stay the durable record, this file is the punch list.

Method: read all 12 files in `concepts/`, both CUE schema files, `SPEC.md`, and
`PROGRESS.md`'s parity/known-gaps sections; then verified every structural claim
directly against `src/`. Every finding cites the line that establishes it.
Findings about the resolver's warning set, start ordering, the container state
machine, and the partial-failure path were cross-checked by a second independent
pass over the same code.

Date of audit: 2026-09-23, against `main` @ `940a31a` plus the uncommitted
working tree (the `actor`/`reducer`/`effects` refactor).

---

## 0. The question, made precise

"Can this describe every system runnable as a local dev env?" is trivially *no*
for any finite language. The useful question is where the boundary sits and
whether it is *principled*. Two separate questions with two different failure
modes:

**Q1 — Expressiveness.** Let `L` be the config language and `R: W → G` the
resolver over workspace disk state `W`. Is the image of `L` under `R` large
enough to cover the intended class of dev environments? Failures here are
*"I cannot say that."*

**Q2 — Soundness.** The system has **four** stores of truth: desired (`G`,
re-derived from disk), planned (`WorkspaceState`), recorded (SQLite + the
sidecar route table), and actual (Docker). Two of them — `W` and Docker —
mutate out of band, by design ([[branch-ownership-model]]: *"fghj is a passive
observer"*). Is every reachable 4-tuple a good state? Failures here are
*"I said it, and the system quietly stopped meaning it."*

### Verdict

No to both — but only one of them is a design problem.

**Q1** fails in a bounded, enumerable way: roughly twenty missing Docker knobs
(mechanical, already tracked in `PROGRESS.md`) plus **eight structural holes**
that need a new concept in the language, not a new field. Two of those (E1, E2)
rule out entire categories of dev environment.

**Q2** fails more seriously, because `concepts/` states several invariants as
*proven* that are in fact only *emergent* — true of the current code by accident
of iteration order, unenforced, and uncheckable. Four confirmed name collisions
that the docs' own stated conventions create, and one state pair
(`desired=running` ∧ `observed=exited`) that nothing in the system can converge.

---

## 1. Tracker

Severity: **S** = structural (needs a new concept, not a field) · **U** =
unsound (reachable bad state or false invariant) · **K** = knob (enumerable,
mechanical) · **D** = doc drift.

| id | sev | finding | status |
|---|---|---|---|
| [E1](#e1) | S | No terminating node kind — seeds/migrations not expressible | open |
| [E2](#e2) | S | Config→run map is a constant function (no parameterization, no local secrets) | open |
| [E3](#e3) | S | Builds cannot carry credentials (`target`/`ssh`/`secrets` absent) | open |
| [E4](#e4) | S | No external / unmanaged dependency kind | open |
| [E5](#e5) | S | No network topology (one network per run, no isolation) | open |
| [E6](#e6) | S | repo→folder map non-injective; `local_path` deleted; silent wrong-repo substitution | **partial** |
| [E7](#e7) | S | Flow names unqualified in a language that qualifies everything else | open |
| [E8](#e8) | S | `version: "1.0"` literal in a federated language — no version negotiation | closed |
| [K1](#k1) | K | ~20 absent Docker knobs (incl. TCP-only ports, no `entrypoint`, no resource limits) | open |
| [B1](#b1) | U | Named port and backing dependency can claim the identical domain | **closed** |
| [B2](#b2) | U | `container_name` collapses the dots that made `node.id` injective | **closed** |
| [B3](#b3) | U | Volume names are a flat cross-repo namespace — silent data sharing | **closed** |
| [B4](#b4) | U | Workspace identity in domains is the folder name; routing scans all workspaces | **closed** |
| [B5](#b5) | U | "The service disappeared" has no representation (`Orphaned` ≡ `Unknown`) | **closed** |
| [B6](#b6) | U | `desired=running` ∧ `observed=exited` is a permanently stuck pair | open |
| [B7](#b7) | U | `ensure_running` partial failure leaves containers invisible to the host | **closed** |
| [B8](#b8) | U | CUE is a linter, not a type system — runtime runs a more permissive language | closed |
| [B9](#b9) | U | Nine resolver warnings, none fatal, none read before a start | closed |
| [B10](#b10) | U | Dependency cycles detected nowhere at start time | closed |
| [B11](#b11) | U | One malformed `.fghj.yaml` aborts resolution for the whole workspace | closed |
| [B12](#b12) | U | Workspace-wide `action_lock` held across a 120 s health wait | closed |
| [B13](#b13) | U | A new commit on the same branch is invisible to drift detection | open |
| [B14](#b14) | U | `ensure_running` never reconciles configuration, only liveness | open |
| [B15](#b15) | U | `daemon_log` could drop a log line under concurrent writers | closed |
| [D1](#d1) | D | Four stale claims in `concepts/` (see §4.1) | partly closed |
| [D2](#d2) | D | Five subsystems in code with no concept doc (see §4.2) | partly closed |
| [D3](#d3) | D | Five concepts that never existed (failure semantics, identity algebra, …) | open |

Suggested order of attack is in [§5](#5-suggested-order).

---

## 2. Expressiveness: what cannot be said

### 2.1 Structural holes

<a id="e1"></a>
#### E1 · No terminating node kind — seeds and migrations are not expressible

The sharpest hole. The language has exactly two node kinds with a lifecycle
(`#Service`, `#BackingDependency`) and both denote *long-running processes*. The
only ordering primitive is `wait_for_healthy`, which gates on a dependency
reporting Docker-`healthy` — a predicate an exited container can never satisfy.
There is no `service_completed_successfully`, no exit-code-gated step, no
one-shot node.

A database seed therefore cannot be a graph node. It cannot be ordered, it
cannot gate a dependent's start, it is not idempotent, it is not recorded, and
it does not re-run. The only mechanism that exists is `fghj exec` — a
full-duplex interactive WebSocket relay requiring an already-running container
and *a live human*. There is no non-interactive programmatic exec. Practically:
a developer types the seed command by hand after the environment is up, every
time, and nothing in the system knows it happened.

Closing this needs a third node kind and a second termination predicate in the
ordering algebra — not a field.

> `src/runs/health.rs:14` · `src/runs/start_node.rs:258` · `src/daemon/api/exec.rs:49`
> · exhaustive search for `seed|migrate|job|hook|post_start` returns nothing

<a id="e2"></a>
#### E2 · The config→run map is a constant function

[[branch-ownership-model]] removed per-run overrides *entirely*, and there is no
host-environment interpolation — the only templating is the bespoke
`${FGHJ_SERVICE_FQDN}` expansion. So for a fixed workspace state there is
exactly *one* environment you can run, modulo `run_id`. Two simultaneous runs
are necessarily identical.

That rules out: running the same stack twice with one env var changed, A/B
config comparison, matrix testing, feature-flag variants per flow, and — most
pointedly — **getting a developer's personal credential into a container**.
Compose's `TOKEN=${TOKEN}` has no equivalent, and `env_file` resolves against
the repo checkout root, i.e. a committed path. There are no Docker secrets
either.

The concept doc argues the removal well on *permission* grounds. But it removed
the only variability mechanism and put nothing in its place, and records that as
a bug fix rather than as a deliberate collapse of the run space.

> `src/runs/fqdn_template.rs` · [[branch-ownership-model]] "Per-run branch pin … removed entirely"

<a id="e3"></a>
#### E3 · Builds cannot carry credentials — in a tool built for private multi-repo orgs

`#Build` is exactly `{context, dockerfile, args}`. No `target`, no `secrets`, no
`ssh`, no `cache_from`.

The irony is structural: [[persistence-and-workspace-store]] describes an
elaborate `WorkspaceOwner` mechanism to forward the user's ssh-agent into
`git clone` — then none of it reaches `docker build`. Any Dockerfile doing
`RUN --mount=type=ssh` or pulling from a private npm/Go/Cargo registry **cannot
be built by fghj at all**. For the stated target audience (a company with ~15
private sibling repos) this is likely the most common hard blocker, and it
appears nowhere in `concepts/` or in `PROGRESS.md`'s parity list.

Missing `target` is separately painful: `target: dev` vs `target: prod` off one
multi-stage Dockerfile is the standard pattern, and here it forces a second
Dockerfile.

> `src/resolver/config.rs:29` · `src/runs/node_spec.rs:146` (`build_image` takes only dir, dockerfile, tag, platform)

<a id="e4"></a>
#### E4 · No external or unmanaged dependency

Every backing dependency is provisioned by fghj from an image. There is no way
to say "postgres already runs at `localhost:5432`, use that," or "this env talks
to the team's shared staging Redis." `extra_hosts` gets you a name mapping but
no node — so no graph presence, no health gating, no drift status, no UI.

"Bring your own" is extremely common in real dev environments, and the omission
also forecloses incremental adoption: a team cannot put half a stack under fghj
and point at their existing Compose stack for the rest.

<a id="e5"></a>
#### E5 · No network topology

Exactly one Docker network per run, name hardcoded as
`fghj-{workspace}-{run_id}`; `RunOpts.network` is a single `&str` with no schema
field behind it. So: no multiple networks, no external network to join, no
`network_mode`, and — the interesting one — **no way to express that A must
*not* reach B**. Any dev environment whose purpose is testing isolation,
partition behaviour, or mesh policy is outside the language.

> `src/runs/orchestrate.rs:27,104` · `src/docker.rs:222`

<a id="e6"></a>
#### E6 · The repo→folder map is non-injective, and the override was deleted

A repo's workspace folder is `repo_name_from_url` — last URL path segment,
`.git` stripped. Host and org are discarded. `github.com/org-a/api` and
`github.com/org-b/api` both map to `<ws>/api`.

Two compounding facts make this worse than a name clash. First, `local_path` —
which [[flat-workspace-model]] still documents as the escape hatch (*"overridable
via `local_path`"*) — **no longer exists anywhere in the schema**, so there is no
override. Second, the clone path is:

```rust
let dest = workspace.join(local_path);
if dest.exists() {
    return Ok(());          // ← silent success
}
```

So pulling `org-b/api` into a workspace that already holds `org-a/api`
**reports success**, and the resolver then reads `org-a`'s `.fghj.yaml` as
though it were `org-b`'s. The `pull_all` fixpoint still terminates —
`downloaded` flips true — so it converges, confidently, to the wrong graph.
There is no check that the existing directory's `origin` matches the requested
URL. That one-line check is the immediate fix; the deeper fix is qualifying the
folder by org.

This also undercuts the uniqueness argument in [[node-identity-and-domains]],
which rests the entire id scheme on folder names being "guaranteed unique
because `scan_workspace` … is a real directory listing." True — but only because
the colliding repo *cannot be present*, which is a limitation dressed as an
invariant.

> `src/downloads.rs:181` · `src/resolver/repo_url.rs:5` · `local_path` absent from `schema/*.cue` and `src/resolver/*`

**Resolution (partial).** The silence is fixed; the non-injectivity is not.
`downloads::ensure_checkout_is` now reads the existing folder's `origin` and
compares it to the requested URL under `normalize_repo_url` (so the same repo
spelled over SSH and HTTPS still matches), refusing a mismatch instead of
returning `Ok(())`. A directory with no readable `origin` — an empty folder, a
half-finished clone — is refused too: it can't be vouched for either, and
assuming it is the requested repo is the same mistake in a quieter form. The
stale `local_path` claim in [[flat-workspace-model]] and the "guaranteed
unique" claim in [[node-identity-and-domains]] are both corrected to say what
actually holds.

Still open: two repos whose URLs share a last path segment cannot coexist in one
workspace at all. The failure is loud now rather than wrong, which is the
important half, but the limitation is real. The deeper fix is qualifying the
folder by org (`<ws>/org-a/api`), which touches every id, every derived domain
and every persisted route — a `local_path`-style override is the cheaper escape
hatch if this turns out to bite. Note also that an error here aborts
`pull_all_logged`'s whole fixpoint via `?`, so one collision blocks pulling the
rest of the workspace. That is [B11](#b11)'s no-error-isolation shape, and
[B11](#b11) is now closed — but only for `scan_workspace`. The clone fixpoint
is a separate call site with the same defect and is **still open**: it wants
the same treatment (collect per-repo failures, keep going, report them all).

<a id="e7"></a>
#### E7 · Flow names are unqualified, in a language that qualifies everything else

[[node-identity-and-domains]] spends its whole length on why ids must be
leaf-first and unconditionally qualified — two departments each naming a service
`bff` must not collide. Flow names get none of that treatment:
`for (flow_name, flow) in &component.flows` takes the raw map key, and flows
live in one flat global namespace across every repo in the workspace.

Two peer repos both declaring `checkout-flow` silently *merge* into one flow
whose membership is the union of two unrelated journeys. Under
[[fog-of-war-visibility]] this is invisible: every node renders anyway, just
highlighted. Same problem, same paper, opposite answer — and the inconsistency
is not acknowledged anywhere.

> `src/resolver/universe.rs:49`

<a id="e8"></a>
#### E8 · `version: "1.0"` is a literal, in a federated language

The CUE pins `version` to exactly one string. The whole premise of the design is
that no repo owns the config — every repo is a peer, each ships its own
`.fghj.yaml`, and they resolve into one graph. Those two facts are incompatible:
there is no way for one repo to adopt a v1.1 feature while its peers are still
on v1.0, because they must all resolve together. **The language has no
version-negotiation story**, so any breaking evolution requires every repo in
every workspace to change at once — reintroducing precisely the
central-coordination problem the federated model exists to abolish.

At runtime this is moot in the worst way: see [B8](#b8) — serde never checks the
value.

**Resolution: major is a compatibility barrier, minor is not.**
`resolver::version::Version` parses `major.minor` and is checked by serde like
any other field.

- A **different major** is refused outright — the file says something this build
  would read differently, and guessing is worse than refusing. It arrives
  through [B11](#b11)'s per-repo isolation, so one repo on a future major does
  not take the workspace down.
- **Any minor** is accepted, including one newer than this build implements.
  That is the part that answers E8: a repo can adopt a 1.1 feature while its
  peers stay on 1.0, and they still resolve into one graph. No flag day.

Accepting a newer minor *silently* would be its own failure — features added
after this build's minor are simply absent, and the config would read as if it
were honoured — so `scan_workspace` pairs acceptance with an advisory warning
naming the repo and both versions.

`schema/component.cue` was widened from the literal `"1.0"` to `^1\.[0-9]+$` to
match, since a schema that rejects a file the daemon accepts would reimpose the
flag day through `fghj validate` instead. `the_schema_accepts_the_same_majors_this_build_does`
checks that the two stay aligned.

What is *not* solved: there is still no way to declare "I need feature X" and
have resolution fail cleanly on a build that lacks it. Minor-version skew is
handled by ignoring-and-saying-so, which is the honest cheap answer, not a
capability negotiation.

<a id="k1"></a>
### 2.2 K1 · Knob gaps — enumerable, mechanical

Every one confirmed absent from both `schema/*.cue` and the serde structs. Most
are already on `PROGRESS.md`'s "eventually" list; none require a design
decision.

| capability | consequence for a real dev env |
|---|---|
| `entrypoint` | Only `CMD` is overridable; changing `ENTRYPOINT` requires editing the Dockerfile. |
| cpu / memory limits | No `HostConfig.Memory` or `NanoCpus` anywhere. A JVM service can OOM the laptop; "this env fits in 8 GB" is unsayable. |
| `tmpfs`, `ulimits`, `sysctls`, `shm_size` | `shm_size` in particular blocks stock Chrome/Postgres images. |
| `devices`, GPU | Any ML/CUDA dev env is out. |
| `stop_signal`, `stop_grace_period` | `stop_and_remove` is unconditional `force(true)`. No graceful shutdown; a DB can be SIGKILLed mid-write. |
| ports: no protocol | `ports` keys match `^[0-9]+$` — **TCP only, no `/udp`**. DNS-based apps, statsd, syslog, QUIC, game/VoIP servers cannot publish. |
| external named volume | Every volume name is *derived*; no way to reference a volume that already exists. |
| `security_opt`, `read_only`, `init`, pid/ipc/uts mode, `logging`, `dns_search`, `pull_policy`, `hostname`, `stdin_open`/`tty` | Individually minor; collectively the long tail. |

**Deliberately out of scope, and correctly so:** the no-host-process decision
(`PROGRESS.md`, 2026-09-07) is a legitimate domain restriction with a stated
rationale, not a gap. It should be written up in `concepts/` as a boundary of
the language rather than left as a changelog line — it is exactly what a reader
needs to find when asking this audit's question. Tracked under [D3](#d3).

---

## 3. Soundness: reachable bad states

### 3.1 One injective id, four non-injective projections

The most serious cluster, and it falls directly out of the convention
[[node-identity-and-domains]] argues for. `node.id` *is* injective — a dotted
chain of dot-free labels, unambiguously parseable. But every namespace derived
from it loses that property, and nothing checks any of them.

<a id="b1"></a>
#### B1 · A named port and a backing dependency can claim the same domain

The two formulas, both stated in the concept doc as the same leaf-first
convention "all the way down":

```
backing node id  = {dep.name}.{owner_id}
  → domain       = {dep.name}.{owner_domain}

named port alias = {port.name}.{node domain}      // start_node.rs:213
  → domain       = {port.name}.{owner_domain}
```

They are the same string. A service `cart` in repo `shop` that has a backing
dependency named `minio` *and* a port named `minio` produces two nodes both
claiming `minio.cart.shop.<ws>.fghj.internal` — one network alias, one DNS name,
one cert, one route. `resolve_route` is a `find_map` over a `BTreeMap`: first
match by key order wins, silently.

The cause is precise: two independently-generated label chains are concatenated
into one flat DNS codomain with no discriminating segment. The docs' own claims
are the opposite — `domain.rs` asserts "Two nodes can never collide on the
result" and `port.rs` asserts a named port "can never collide across
runs/workspaces either." Both are true across runs and workspaces and false
within a node. `check_port_config` catches only duplicate `primary` and orphan
`wildcard`; there is no domain-collision check at all.

> `src/runs/start_node.rs:213` · `src/resolver/visit_dependency.rs:196` · `src/state/query.rs:68` · `src/resolver/validate.rs`

**Resolution (closed, with [B2](#b2) and [B4](#b4)).** New pass
`resolver::uniqueness::check_derived_name_collisions`, run at the end of
`resolve_universe` — after `Node.domain` is filled in, which is why it can't
live in the traversal. It builds the *actual* codomain `resolve_route` scans —
each node's own domain, its `{name}.{domain}` named-port aliases, its
`additional_hosts` and its `wildcard_hosts`, all in one map — and warns on any
name claimed twice, naming both nodes and what made each claim it. The
duplicate-`wildcard_hosts` check that used to sit inline in `resolve_universe`
was one special case of this and is now subsumed; an `additional_hosts` entry
colliding with a derived domain was checked nowhere before and now is.
Not-yet-pulled service stubs are excluded: their config is unknown, so any
collision they appear to have is an artifact.

The escaping asymmetry [B2](#b2) names is checked in the same pass, separately,
because it is a different failure: `sanitize_label(node.id)` collisions don't
misroute, they make Docker refuse the second container with an error that never
mentions naming.

These are warnings, matching every other resolver finding — and they are
`Severity::Blocking` ones, so since [B9](#b9) they refuse the start rather than
just decorating the banner.

<a id="b2"></a>
#### B2 · Container names collapse the id's dots

```rust
container_name = format!("fghj-{workspace}-{run_id}-{}", sanitize_label(&node.id));
// sanitize_label collapses every non-alphanumeric run to a single '-'
```

Service names permit hyphens, so a service `a-b` in repo `c` (id `a-b.c`) and a
backing dep `a` on service `b` in repo `c` (id `a.b.c`) both sanitize to
`a-b-c`. Docker refuses the duplicate name, so the second node simply fails to
start — a partial-start failure caused entirely by naming, with an error message
that will not mention the real cause.

Note the asymmetry: `derive_domain` does *not* sanitize `node_id` (dots
preserved, injectivity kept) while `container_name` does (dots destroyed). The
same id flows into two projections with opposite escaping rules.

> `src/runs/node_spec.rs:36` · `src/util/label.rs:6` · `src/runs/domain.rs:42`

**Resolution (closed).** Checked by `uniqueness::check_container_names`, in the
pass described under [B1](#b1).

<a id="b3"></a>
#### B3 · Volume names are a flat, unqualified global namespace — documented as a feature

`derive_volume_name` is keyed on the author-chosen `name`, not on any node id.
[[docker-and-downloads]] presents this as intentional sharing: "two unrelated
nodes … that declare the same `name` + `scope` derive the same value and
transparently share one Docker volume."

The sharing is real and useful for `shared-backing`. The problem is that it is
*unbounded and cross-repo*. Two unrelated peer repos that each declare
`{name: data, scope: stable}` on their own Postgres get **one volume, and two
database engines writing to it**. This is silent data corruption, reachable from
two valid configs written by two teams who have never spoken — which is the
exact scenario the entire id-qualification scheme exists to prevent. Volume
names are the one namespace in the design that is not qualified, and the doc's
phrasing ("transparently") obscures that it is also the one with a destructive
failure mode.

> `src/runs/naming.rs:25` · [[docker-and-downloads]] "Volumes: two shapes, one Docker primitive"

**Resolution (closed).** Both halves of the suggested remedy, since they are
the same change seen from two sides. `derive_volume_name` now takes the
declaring node's id and folds it in, making the author-chosen label private to
that node by default — the leaf-first qualification the rest of the system
applies everywhere, finally applied here. `#Volume.shared: true` opts back out,
dropping the qualification so the label alone decides identity.

`shared` defaults to `false`, and the schema comment points anyone reaching for
it at `#SharedBackingDependency` instead: sharing a database is one *node*, one
container, one volume — no cross-node name coincidence required. The two modes
also derive different names, so flipping the flag is a visible migration rather
than a silent adoption of someone else's data.

Migration note, deliberately not automated: volumes created before this change
are orphaned rather than re-pointed. That is dev-environment data and a
re-seed, and inventing a rename that guesses which node used to own a volume
would be a worse failure than an obviously-empty one.

<a id="b4"></a>
#### B4 · Workspace identity in domains is the folder name, but routing scans all workspaces

The domain suffix is `sanitize_label(workspace_name)`, while `resolve_route`
walks *every wired workspace*. Two workspaces whose names sanitize identically —
e.g. the same project checked out at `~/work/shop` and `~/scratch/shop`, which
is a first-class use case given review runs — produce identical domains, and
traffic goes to whichever appears first in `BTreeMap` order. Workspaces are
keyed by *id* in the index but by *name* in the domain.

**Resolution (closed).** `WorkspaceRegistry::resolve` now refuses to wire a
workspace whose folder name sanitizes to the same label as an already-wired
one, alongside the existing nesting check. This has to be a hard error rather
than a warning: once both are wired there is nothing left to disambiguate
with — the colliding segment is all the domain carries — so the only moment the
user can act on it is before the second one exists. Two workspaces wired
*before* this check existed stay wired; the daemon restores the index directly
at startup and deliberately doesn't retroactively unwire anything.

### 3.2 Invariants the docs assert but nothing enforces

Beyond the collisions, several statements in `concepts/` read as proven
properties but are emergent artifacts of the current code. Each would survive a
refactor only by luck.

| claim in `concepts/` | actual status |
|---|---|
| "Only one run can actually own a `stable`-scoped name from the host at a time." ([[node-identity-and-domains]]) | **Not enforced.** Both runs register the same route; `resolve_route`'s `find_map` returns the first by map order. Nothing rejects the second run, nothing warns, and which run you reach is unspecified. |
| "Two nodes can never collide on the result." (`src/runs/domain.rs`) | **True of the formula, false of the codomain** — see [B1](#b1), now warned on at resolve time. |
| "The folder name (`local_path`) is guaranteed unique…" ([[node-identity-and-domains]]) | `local_path` no longer exists. The guarantee now reads: two same-named repos *cannot both be present* — see [E6](#e6). |
| "A dependency whose local folder isn't present renders as a stub instead of failing." ([[flat-workspace-model]]) | **True**, and now true of a malformed one too, in a different way — see [B11](#b11). |
| Diamond branch conflicts are "structurally impossible." ([[branch-ownership-model]]) | **True** — and the strongest argument in the folder. Worth noting the cost: the conflict is not resolved, it is made *unrepresentable*, so a genuine need for two branches of one repo must be serialized in wall-clock time with no mechanism to help. |

### 3.3 The mutable-world problem

The code is now *ahead* of the docs here: `config_drift` / `DriftReport` /
`SyncStatus` exist, a second reconciler re-resolves every 15 s, and the UI has a
desired-vs-actual indicator. `concepts/` documents none of it, and `PROGRESS.md`
still lists it as an open gap. Tracked under [D2](#d2).

What drift detection does *not* cover is the branch-switch case:

1. `bff.shop` is running in the default run, routed, holding its domain and a `stable`-scoped volume.
2. You `git switch` in `<ws>/shop`. The new branch's `.fghj.yaml` does not declare `bff`.
3. Within 15 s the sync reconciler re-resolves. `bff.shop` is gone from `G`. `config_drift` does `graph.nodes.iter().find(...)` → `None` → `synced: None` → `SyncStatus::Unknown`.
4. `Unknown` is, by explicit design, *the same value* as "no drift check has run yet." The comment says so: the conflation is deliberate.

<a id="b5"></a>
#### B5 · "The service disappeared" has no representation

The system's three-valued sync lattice is `Unknown | Synced | Drifted`, and the
state in question — *a running container whose node no longer exists in the
desired graph* — is folded into `Unknown` alongside "haven't looked yet." So the
answer to "after a branch switch a service can disappear and we don't know what
to do" is sharper than it first looks: **the system cannot currently tell you it
happened.** An orphaned container keeps running, keeps its route, keeps its
volume, and reports the same status as a freshly-started one nobody has
inspected.

The fix is small and the design already has the shape for it: a fourth variant
(`Orphaned`) distinguishing "node absent from `G`" from "not yet checked", plus
a UI affordance. The policy question — stop it, keep it, ask — can stay the
user's, exactly as the read-only reconciler philosophy demands. But the
*observation* has to be representable before any policy can exist.

> `src/state/sync_status.rs:15-20` · `src/runs/observe.rs:175-181` · `src/daemon/reconcile.rs:81`

**Resolution (closed).** `SyncStatus::Orphaned` added as a fourth variant.
`runs::observe::config_drift` now returns it when `graph.nodes` has no entry for
a live container, and `DriftReport` carries the full `SyncStatus` rather than the
`Option<bool>` that made `Orphaned` and `Unknown` indistinguishable before the
reducer ever saw them. The Actual tab synthesizes a graph node for any orphaned
container so it stays selectable — and therefore stoppable — after its
declaration is gone; the drawer names it "no longer declared." Policy is
unchanged and deliberately so: nothing stops, removes, or reconciles an orphan.
See [[run-lifecycle-and-registry]]'s "When a node disappears".

The one lossy edge: `Orphaned` narrows to SQL `NULL` on persist and restores as
`Unknown`, same as it always did for `Unknown`. That is correct rather than
merely tolerable — across a daemon restart no drift check has run yet, so
`Unknown` is the honest reading, and the next sync tick recomputes it.

<a id="b13"></a>
#### B13 · A new commit on the same branch is invisible

The drift hash includes the image tag `fghj/{id}:{branch}` but has no content or
SHA component. Commit, don't switch branches, and the running container stays lit
as `Synced` while serving a stale image. A branch *switch* is caught only
incidentally, because the tag changes.

<a id="b14"></a>
#### B14 · `ensure_running` never reconciles configuration

It checks liveness only, so a container that *is* flagged `Drifted` will not be
recreated by "Run flow" — the top-up skips it because it is alive. Drift is
observed and then, by explicit policy, ignored. That policy is defensible; it
should be written down in `concepts/`, because right now the only place it exists
is a comment.

<a id="b15"></a>
#### B15 · `daemon_log` could silently drop a line under concurrent writers

Not in the original review — surfaced by a flaky
`tail_without_after_seq_respects_the_limit_and_stays_ordered` while closing
[B8](#b8), and worth more than the flake suggested.

`push` allocated the sequence number *outside* the buffer lock:

```rust
let seq = state.next_seq.fetch_add(1, Ordering::Relaxed);
let mut entries = state.entries.lock().unwrap();   // another writer can win here
```

So two concurrent writers could take seq 5 and 6 and append them in the other
order, leaving the buffer holding `[.., 6, 5]`. Out-of-order rendering in the
telemetry drawer's Logs tab was the harmless half. The real damage is that
`tail(Some(after), ..)` filters on `e.seq > after`: a poller that had already
seen 6 would filter 5 out **forever**. A log line silently lost, which is the
one failure a log must not have. `fghjd` has several concurrent writers — the
converge tasks (including the one [B7](#b7) added), DNS, `raw_net` — so this
was reachable in ordinary operation, not only under test.

**Resolution.** The counter moved *inside* the mutex, as one `Mutex<Log>`
holding both the `VecDeque` and `next_seq`, so seq order and insertion order
are the same thing by construction rather than by remembering to keep them so.
`concurrent_writers_cannot_append_out_of_seq_order` asserts the invariant
globally across 8 threads; it was confirmed to fail against the old
implementation before being kept.

<a id="b6"></a>
#### B6 · A permanently stuck state pair

`desired.running == true` ∧ `observed.status ∈ {exited, removed}` is reachable
(container crashes, or is `docker rm`'d) and **nothing converges it**.
`DockerConvergeEffect::extract` reads only `pending_action` / `pending_create`,
never `observed`; the reconciler is read-only by design; `AutoHeal` is
constructed nowhere. Only an explicit user Start clears it.

Read-only-by-default is a good decision — [[run-lifecycle-and-registry]] argues
it well against a Kubernetes controller. But "the user decides" only works if the
user is *told*, and a stuck desired/observed pair currently has no distinct
presentation either. Same missing-vocabulary problem as [B5](#b5).

Related: Docker states `restarting`, `paused`, `dead`, `created` are reachable
but nothing branches on them; every consumer tests `== "running"`, so a
restarting container is silently de-routed and indistinguishable from an exited
one.

### 3.4 Partial failure

<a id="b7"></a>
#### B7 · Node 3 of 5 fails on the default run: two containers become invisible

The two start paths behave completely differently, and the common one is the
unsafe one.

**Named run** (`start`): full rollback — every started container removed,
sidecar removed, network removed. Clean.

**Default run** (`ensure_running` — every "Start default environment" and "Run
flow" click): a bare `?`. No rollback. Nodes 1–2 stay running and are persisted
to SQLite and the sidecar route table after each node — deliberately, so
progress isn't lost. But they were never inserted into
`WorkspaceState.runs[...].containers`, because `mirror_run` only runs on the
success path, and `ContainerObserved` is a silent no-op for a node not already
in the map.

Result: two containers that are running, persisted, and reachable from inside
the docker network — but **absent from `GET /runs`, absent from the UI, and
unroutable from the host**, because `resolve_route` reads the new state only.
Two stores of truth now disagree and nothing reconciles them. Meanwhile the HTTP
client already got its `200` before convergence ran, and the error string is
discarded without logging — the only trace is the per-node events table.

The clearest case in the codebase of the four-store problem from §0 producing a
state no single component can see.

> `src/runs/orchestrate.rs:180` · `src/effects/docker/converge.rs:239,247` · `src/reducer/observation.rs:31-40,82-86`

**Resolution (closed).** Not by rolling back — that is the one fix that would
be wrong here. `start` builds a *named* run from nothing, so a half-built run is
garbage and removing it is right; `ensure_running` tops up the single shared
default environment, where three containers coming up is a real outcome someone
wants, and tearing them down because the fourth failed would destroy exactly
the progress the per-node `save_run` exists to preserve. The defect B7 names is
not the missing rollback, it is the *invisibility*.

So the error now carries the progress. `Action::RunCreateSettled`'s error side
became `state::RunCreateError { message, partial: Option<RunState> }`;
`ensure_running` fills `partial` with the run as of the failure, and the reducer
adopts it (clearing `pending_create` explicitly, since a working copy doesn't
arrive clear). `start`'s path leaves `partial: None` via
`From<anyhow::Error>` — it really has nothing outstanding — so the two paths
now say two different true things instead of one path saying nothing.

The discarded error string is also fixed: `spawn_create` logs the failure and
how many containers did come up through `daemon_log::warn`, since the HTTP
caller got its `200` long before any of this ran.

### 3.5 Validation

<a id="b8"></a>
#### B8 · The CUE schema is a linter, not a type system

`schema/*.cue` is enforced only by `fghj validate` — an opt-in CLI command that
shells out to an external `cue` binary which may not be installed. **The daemon
never invokes it.** The runtime boundary is serde, which is strictly more
permissive: `#[serde(default)]` throughout, untagged enums, `version: String`
with no literal check, and none of the `=~"^[a-z0-9][a-z0-9-]*$"` constraints
that the whole naming argument depends on.

So the invariants in [[node-identity-and-domains]] hold over `L_cue`, while the
system actually runs `L_serde`, and the two are never proven equal. Concretely:
a service named `"My Service!"` passes serde, flows unsanitized into `node.id`,
and from there into a DNS name and a network alias that are simply invalid —
while the container name, which *is* sanitized, works fine. Unvalidated input
reaches three namespaces with three different escaping disciplines.

Either the resolver should validate against the schema at load, or the Rust
structs should carry the constraints as parse-time newtypes. As it stands, "the
CUE shapes are the source of truth" (`concepts/README.md`) is aspirational.

**Resolution — and the framing changed.** The choice was put as "enforce CUE at
load, or move the constraints into Rust", and the answer settled something
prior: *CUE is for users, not for the daemon.* It is a self-check for the person
writing the file, their agent, or CI. The daemon must enforce everything
internally, on its own.

That makes the direction of authority the opposite of what `concepts/README.md`
claimed. Rust is the source of truth; the schema is downstream. Enforcing CUE at
load is also independently unworkable — it would shell out to an external `cue`
binary per `.fghj.yaml` on every resolve, and the daemon re-resolves on a poll,
so it is a subprocess storm *and* makes `cue` a hard runtime dependency on every
dev machine rather than a `fghj validate` one.

So `resolver::name::Name` is a newtype whose `Deserialize` enforces
`^[a-z0-9][a-z0-9-]*$`, applied to every field that reaches a node id: service
map keys, flow `service`, backing `name`, `kind: service`'s `services:` list,
shared-backing `service`/`name`, port `name`, volume `name`. Holding a `Name` is
proof it is safe in a DNS label, a Docker network alias and a container name
without further escaping — the constraint is the *intersection* of what those
three accept, not the union, so nothing downstream escapes and no two escapings
can disagree. There is deliberately no `From<&str>`: the only ways in are
`Deserialize` and `Name::parse`, both of which validate.

`"My Service!"` — B8's own example — is now rejected at parse time with
`'M' is uppercase; names are lowercase because they become DNS labels`. Because
of [B11](#b11), that surfaces as a blocking warning naming the repo, file and
line, while its peers resolve normally.

The schema's remaining obligation is to not *lie*: a file that passes `fghj
validate` must not then be rejected by the daemon. `rust_and_cue_agree_on_what_a_name_is`
enforces that by reading `schema/*.cue`, extracting every `=~"…"` identifier
constraint, and failing if any differs from the Rust pattern — so "the shapes
agree" is a checked fact rather than a comment asking people to remember.

<a id="b9"></a>
#### B9 · Nine warnings, none fatal, none read

There are nine resolver checks. **All nine are non-fatal and nothing consumes
them before starting a run** — `start`, `ensure_running`, `restart_container`
and `perform_create` all take `&Graph` and never read `.warnings`. The only
consumer is a UI banner. So a config with two primary ports, dangling
shared-backing refs, and duplicate wildcard suffixes starts happily.

Also absent: no exact-`additional_hosts` collision check (only wildcard
suffixes), no duplicate-`host_port` check across nodes, no check that a
`#Service` node actually has a `build`.

**Resolution.** A warning is no longer a bare `String`. `resolver::Warning`
carries a `Severity` — `Blocking` or `Advisory` — and `Graph::refuse_if_blocked`
turns every blocking one into a refusal, called from both start paths
(`perform_create` for a whole run, `perform`'s `Starting` arm for one node).
The error names *all* of them at once rather than the first, because they are
usually independent mistakes and fixing them one attempt at a time is
miserable.

Resolution itself still never fails, and that is the point of the split: a
workspace you cannot fully understand has to remain *viewable* so you can see
what is wrong with it — refusing to resolve would blank the screen exactly when
you need it. Viewing is not actuating, so the refusal lives at the start path,
not in the resolver.

The classification is by "can this instruction be carried out as written":

- **Blocking** — a self-dependency, a dependency naming a service that does not
  exist, a multi-service repo referenced without `service:`, more than one
  `primary` port, a dangling `shared-backing` reference, and both derived-name
  collision checks from [B1](#b1)/[B2](#b2). Each of these means some part of
  the declared graph cannot exist; starting anyway brings up a subset nobody
  asked for and reports success.
- **Advisory** — a repo that declares no services, a `wildcard` port with
  nothing to wildcard, `additional_hosts` with no primary port, and cycles (see
  [B10](#b10)). These describe something inert, not something wrong.

The UI reads `severity` too: advisories are muted, blocking ones keep the loud
treatment and are the only kind that lights the header's conflict indicator.

Still open, deliberately:

- The three *absent* checks listed above. Adding a check is now cheap — the
  severity machinery is what was missing.
- The gate is **workspace-wide, not per-node**. `Warning` carries a message and
  a severity, not the node id it is about, so restarting one healthy container
  is refused while an unrelated repo has a blocking config error. That is
  coarse, and on a small shared workspace it is the safe direction to be coarse
  in — but attributing each warning to the node(s) it concerns, and gating
  `perform`'s single-node path on only those, is the better end state.

<a id="b10"></a>
#### B10 · Dependency cycles are detected nowhere

[[branch-ownership-model]] explicitly acknowledges that real requirement cycles
are possible. At start time:

```rust
if !progressed {
    ordered.extend(remaining.into_iter().map(str::to_string));
    break;                                  // src/runs/order.rs:51
}
```

Cyclic nodes are appended in alphabetical order and started anyway — no error,
no warning. The doc comment says cycle resolution is "independent of `fghj
validate`", but `validate` is CUE-only and CUE cannot express a cross-repo cycle
either. The graph layout code defends against cycles (it drops back-edges to
avoid blowing out the canvas); the thing that actually starts containers does
not.

**Resolution.** `resolver::cycles::check_cycles` runs as a pass in
`resolve_universe`, walking `depends-on`/`owns` edges (the same two kinds the
start order is derived from — a loop through a `shared-backing` cross-reference
is not a start-order problem) and reporting each distinct cycle once, naming
its nodes in the order they depend on each other: `a -> b -> c -> a`, not an
unordered stuck set. Two back-edges around the same loop describe the same
cycle rotated, so it is canonicalized by node set before reporting.

**Advisory, not blocking** — `order.rs`'s fallback is the right call and is
staying. Refusing to bring up an entire environment over a cycle would be worse
than bringing it up in an arbitrary order, and the order is at least stable
(both the fallback and this pass sort). What was actually missing was never the
refusal; it was that *nobody was told*. The start order silently stops being a
dependency order, a service comes up before the database it declared it needs,
and the only symptom is a crash loop with no explanation. Now the loop is named
on screen, and you can decide whether it matters.

<a id="b11"></a>
#### B11 · One malformed `.fghj.yaml` aborts the whole workspace

A *missing* repo stubs correctly, per [[flat-workspace-model]]. A *malformed*
`.fghj.yaml` in *any* repo in the workspace propagates out of `scan_workspace`
and aborts `resolve_universe` entirely — the graph endpoint 500s and every start
fails. One bad file anywhere takes down the whole workspace, which contradicts
the lazy/partial-resolution principle the same doc states.

> `src/resolver/workspace_scan.rs:10-15,41`

**Resolution.** `scan_workspace` now returns `ScannedWorkspace
{ components, warnings }` and skips a repo it cannot read instead of
propagating, one blocking `Warning` per bad repo, each carrying the
`serde_yaml` line and column. `resolve_universe` folds those in alongside
every other finding. The `Result` is now reserved for the workspace
*directory* being unreadable — not a per-repo problem, and nothing is left to
partially resolve.

Skipped, deliberately **not** stubbed. A missing repo legitimately stubs:
fghj genuinely does not know what it declares, and `downloaded: false` says
exactly that. A malformed repo is present and says something fghj cannot
read — stubbing it would substitute "declares nothing" for the author's
actual intent, silently, which is the failure mode [E6](#e6) rejected in the
clone path for the same reason.

So the two halves of B11 land differently, and that is the point:

- **Viewing always works.** The graph endpoint no longer 500s, so you can
  open the workspace that contains the broken file and read the error naming
  the repo and the line.
- **Starting still refuses**, because the warning is `Blocking` per
  [B9](#b9). A present-but-unreadable repo is a hole in the graph, and
  starting around it brings up a subset nobody asked for.

That is the same view/actuate split [B9](#b9) settled, applied one layer
earlier.

<a id="b12"></a>
#### B12 · Workspace-wide lock held across a 120 s health wait

`wait_for_healthy` cannot deadlock — hard-bounded at 60 × 2 s = 120 s. But it is
called *inline* in the sequential per-node loop with no overall timeout, so an
N-node run worst-cases at N × 120 s. Worse, `restart_container` /
`stop_container` / `remove_container` hold `action_lock` — a **workspace-wide**
mutex — across `start_node` → `wait_for_healthy`. One unhealthy node blocks every
other node's lifecycle operation in that workspace for up to two minutes.
`begin_action` dedups only the *same* node.

> `src/runs/health.rs:14-23` · `src/runs/registry.rs:26` · `src/runs/lifecycle.rs:72,116,159`

**Resolution.** Both halves fixed, and they turned out to be one problem seen
twice: the lock and the timeout were each scoped to the wrong unit.

The workspace-wide `action_lock` became `RunRegistry::node_locks`, a
`tokio::sync::Mutex` per `(run_id, node_id)` created on first use. Two calls
for the same node genuinely must not interleave — both `stop_and_remove` and
then recreate one fixed container name, which Docker rejects for the loser.
Two calls for different nodes were never in conflict at all. The old lock
conflated them, so one slow healthcheck froze every Start and Stop button in
the workspace.

Finer locking exposed a hazard the coarse lock had masked: every lifecycle
path cloned the whole `RunState`, mutated the clone across its Docker work,
and wrote it back wholesale, so two concurrent per-node updates would each
erase the other's container. `RunRegistry::commit` now re-reads the live state
under the registry mutex, applies a targeted change, and hands back the
*merged* value — which is what goes to `save_run` and `write_route_table`,
since a route table written from a stale snapshot would unroute a container
that is running fine. `commit` returning `None` for a vanished run is the
honest answer rather than resurrecting what the user just stopped.

For the timeout: the per-node bound was never a bound on anything a waiting
person cares about, because nodes start sequentially. `HealthBudget` is a
deadline shared by every node in one whole-run call; each node gets whatever
is left, capped at the per-node limit so one node cannot eat the run's
allowance. Running out truncates the **wait**, not the run — the same
best-effort call `wait_for_healthy` already made at its own limit, since
refusing to bring up the rest of an environment because a clock expired turns
a slow start into no environment. A truncated wait records a distinct event
rather than a clean pass: a node fghj never saw report healthy must not look
identical to one it did.

The whole model is now written down in [[concurrency-model]], which is also
the D3 bullet naming "what `action_lock` protects, what may run concurrently
across runs and workspaces, which locks are held across `await`".

---

## 4. The `concepts/` folder as an artifact

Separately from what it says: the folder declares itself "the durable design
record" and "required reading before proposing an architecture change." It has
drifted, and the drift is load-bearing because the folder is meant to be read
*instead of* the code.

<a id="d1"></a>
### 4.1 D1 · Stale — states things that are no longer true

- ~~`README.md`'s "Map of the codebase" points at `src/resolver.rs`,
  `src/runs.rs`, `src/daemon.rs`, `src/store.rs` — **all four no longer exist**
  (split into directories).~~ **Closed.** The map now names the real paths, and
  the run row was split in two so `src/runs/` and the actor stack point at
  different guides.
- [[flat-workspace-model]]: "overridable via `local_path`" — `local_path` is gone
  from the schema entirely.
- [[node-identity-and-domains]]: rests its uniqueness argument on `local_path`;
  the `derive_domain` signature quoted in the doc is missing the `zone`
  parameter.
- [[docker-and-downloads]]: the named-volume-leak gap it describes was closed.

<a id="d2"></a>
### 4.2 D2 · Missing — subsystems in code with no concept doc

- The **in-network TLS sidecar** (one proxy container per run) — documented in
  `docs/src/content/docs/concepts/sidecar.md` but absent from `concepts/`. The
  two folders have forked.
- The **dual DNS zone** split (`fghj.internal` vs `fghj.raw.internal`) — this
  changes the central story of [[split-dns]] and [[node-identity-and-domains]],
  both of which still describe one zone.
- **`raw_net`**: per-node virtual IPs NAT'd via `pf`, with a hash-based allocator
  that can collide and a sticky in-memory reconciler on top. A substantial new
  invariant surface with zero concept coverage.
- ~~The **actor / action / reducer / effects architecture** (`src/action.rs`,
  `src/reducer/`, `src/effects/`, `src/state/`) — the single largest structural
  change in the codebase, described nowhere. [[run-lifecycle-and-registry]] still
  describes the pre-refactor `RunRegistry`.~~ **Closed** by
  [[state-and-effects]], written when the migration finished.

  **Resolution.** The migration was half-done and the half-doneness was the
  expensive part. Two stores of run state existed — the actor's
  `WorkspaceState` and `RunRegistry`'s own `BTreeMap<String, RunState>` —
  reconciled by `effects::bridge` re-polling the second one about once a
  second. Everything that made the codebase feel bigger than "hosts, domains,
  ports and TLS over Docker" traced back to that duplication: every lifecycle
  call wrote both copies, observations could reach SQLite ahead of the reducer
  that owned them, config drift had two fields that disagreed, and duplicate-
  action gating had two answers (`RunRegistry.pending` and
  `ContainerInfo::pending_action`).

  `RunRegistry` is now stateless. It keeps the Docker client, the database
  handle, the per-node locks and the log-capture tasks; it takes whatever
  prior state a call needs as an argument and reports results back as
  `Action`s. `refresh` became `inspect_containers(&runs) -> Vec<..>` —
  reporting, not writing. `list()`, `commit()` and the map itself are gone;
  the one thing `list()` was used for (seeding the actor at startup) is now
  `server::WorkspaceState::rehydrated`, read exactly once. Persistence and the
  sidecar route table stopped being a thing each lifecycle call had to
  remember and became `effects::persist` / `effects::routes`, pure functions
  of published state.

  Two hazards the removal exposed, both fixed here rather than left implicit:
  a daemon dying mid-create would have left containers running and unrecorded
  (fixed by `runs::progress` -> `Action::RunCreateProgress`, reported per node
  as it comes up, with the drain ordered before the settle so a late report
  can't re-add a container the settle dropped); and a create settling *after*
  its run was torn down would have resurrected it, so `RunCreateSettled` now
  writes through the existing entry instead of inserting, matching what
  `RunCreateProgress` already did.
- **Config drift / `SyncStatus`**, `fghj exec`, wildcard hosts, FQDN templating,
  telemetry.

<a id="d3"></a>
### 4.3 D3 · Never existed — concepts the folder has no file for

- **Failure semantics.** What is fatal vs. advisory, what rolls back vs. what
  persists, where an error surfaces. Currently: nine non-fatal checks, one fatal
  parse error with workspace-wide blast radius, two start paths with opposite
  rollback behaviour, and errors dropped after a `200`. All policy, none written
  down.
- **The identity algebra.** [[node-identity-and-domains]] covers ids beautifully
  and then stops. The projections — DNS names, container names, volume names,
  network names, virtual IPs — each have their own escaping rules and their own
  collision domains, and nobody has written the table.
- **The config language's own semantics**: CUE vs serde, what validates where,
  what `version` means, how the language evolves.
- **The security model.** A root daemon, a CA in the system trust store,
  unsandboxed bind mounts by explicit decision, `privileged: true` available to
  any repo you clone, `pf` rules. Each is individually reasoned in its own file;
  the composite threat model — *cloning a repo grants its author these
  capabilities on your machine* — is stated nowhere.
- **The concurrency model**: what `action_lock` protects, what may run
  concurrently across runs and workspaces, which locks are held across `await`.
- **The boundaries of the language**: the no-host-process decision and what else
  is deliberately out of scope (see [K1](#k1)).

---

## 5. Suggested order

Cheap-and-high-value first; design work last.

| # | action | closes |
|---|---|---|
| ~~1~~ | ~~Add `SyncStatus::Orphaned`, distinct from `Unknown`~~ — **done** | [B5](#b5) |
| ~~2~~ | ~~Verify `origin` on an existing clone target~~ — **done** | [E6](#e6) |
| ~~3~~ | ~~A resolver pass checking derived-name uniqueness~~ — **done** | [B1](#b1) [B2](#b2) [B4](#b4) |
| ~~4~~ | ~~Make `ensure_running`'s partial failure mirror the named-run path~~ — **done** | [B7](#b7) |
| ~~5~~ | ~~Qualify volume names, or require explicit opt-in to share~~ — **done** | [B3](#b3) |
| ~~6~~ | ~~Detect cycles at start; gate starts on warnings~~ — **done** | [B9](#b9) [B10](#b10) |
| ~~7~~ | ~~Per-repo error isolation in `scan_workspace`~~ — **done** | [B11](#b11) |
| ~~8~~ | ~~Decide: enforce CUE at load, or move constraints into the Rust types~~ — **done** | [B8](#b8) [E8](#e8) |
| ~~9~~ | ~~Narrow `action_lock` scope / add an overall start timeout~~ — **done** | [B12](#b12) |
| 10 | Design a terminating node kind | [E1](#e1) |
| 11 | Build secrets / ssh forwarding | [E3](#e3) |
| 12 | Refresh `concepts/` — *partly done: the codebase map and the actor-architecture gap are closed by [[state-and-effects]]* | [D1](#d1) [D2](#d2) [D3](#d3) |

Remaining structural items ([E2](#e2), [E4](#e4), [E5](#e5), [E7](#e7)) and the
knob list ([K1](#k1)) are scope decisions rather than defects — each wants an
explicit accept-or-close call recorded in a concept file, not necessarily code.
