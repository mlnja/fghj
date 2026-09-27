# Config drift and `SyncStatus`

## The problem

A container is created once, from the `.fghj.yaml` that existed at that
moment, and then keeps running. The file underneath it does not hold still:
someone edits a port, adds an env var, switches a branch, or deletes the repo
from the workspace entirely. The container knows nothing about any of that.

So there are always two answers to "what does this node look like" — the one
Docker is actually running, and the one the config on disk describes right
now — and the interesting fact is whether they agree. Nothing else in the
system can report that fact: [[run-lifecycle-and-registry]]'s reconciler
compares running against *desired-as-recorded*, which is precisely the stale
copy. Drift is the comparison against the file.

## Nothing in the background acts on drift

fghj detects drift continuously and never acts on it *on its own*. When the
user asks — a top-up, an explicit `POST /runs` — a drifted container is
recreated. The split is not between observing and acting; it is between
**who asked**.

This is the decision worth stating loudly, because the obvious alternative —
recreate the container the moment its config changes — is what every
`docker compose up` does and what people expect. As a *background* policy it
is wrong here. A run may
have a database with hours of seeded state in a `scope: stable` volume, a
long-lived process mid-debug, a terminal attached. Recreating a container
because someone touched an unrelated line of YAML would destroy work the user
never asked to lose, at a moment they were not looking. Worse, the trigger is
often not even an edit: `git switch` in a workspace checkout changes the whole
graph, because fghj is a passive observer of branch state
([[branch-ownership-model]]).

So the observation and the policy are split. The observation must be
*representable* before any policy about it can exist. Nothing feeds a
`Drifted` or `Orphaned` reading back into a reducer decision —
`effects::docker::converge` acts only on explicit intent (`pending_action`,
`pending_create`), never on a drift verdict. The reconciler
(`daemon::reconcile`) computes verdicts and reports them, full stop.

### The one place drift is acted on, and why it is not a contradiction

`RunRegistry::ensure_running` — the top-up — recreates a container whose hash
no longer matches. See `top_up_may_skip`, which is where all three skip
conditions live.

The distinction that makes this consistent rather than a special case: a
top-up is not a tick. It happens because someone pressed "Run flow" or ran
`fghj up`, and the mental model people bring to that is `compose up`'s — *my
edits take effect*. For a long time a drifted container was skipped precisely
**because** it was alive, so editing `.fghj.yaml` and pressing the button did
nothing whatsoever. That was this policy leaking out of the reconciler, where
it is right, into a path where the user had explicitly asked (`AUDIT` B14).

Recreating is narrated as a `create` event with the reason ("config changed
since it was started"), because from the outside a top-up bouncing a
container that was running fine looks like a bug. And recreating now goes
through the graceful `stop_and_remove` described in [[docker-and-downloads]],
which is what makes it safe to do on an ordinary action at all — the earlier
force-kill would have meant answering "my edit didn't apply" by SIGKILLing
the user's database.

An inconclusive re-resolve (`Ok(None) | Err(_)`) counts as *not* drifted.
Nothing has established that the running container is wrong, and bouncing a
healthy one on a guess is the more expensive of the two mistakes.

## The four states

`state::SyncStatus` has four variants, and the fourth one is the whole
reason it is an enum rather than a bool.

| | Meaning |
|---|---|
| `Synced` | The freshly-computed desired hash equals the hash stamped on the container. |
| `Drifted` | They differ. The node still exists; its config changed since it was started. |
| `Unknown` | Nothing conclusive: no check has run yet, or re-resolving this node's spec failed on the last one. |
| `Orphaned` | The container is running and its node is **gone** from the freshly-resolved graph. |

`Orphaned` used to be spelled `Unknown`, which is the bug
`concepts/AUDIT.md` B5 recorded. "This node vanished from the graph" and
"nobody has looked yet" are opposite facts — one is actionable, the other is
the absence of information — and conflating them made the single most common
real-world case (a `git switch` in a checkout, leaving containers behind for
services that branch doesn't declare) indistinguishable from startup noise.
Anything that can act on one has to be able to tell them apart, so they must
be different values, whether or not anything acts on either today.

A resolution failure is deliberately `Unknown` and not `Orphaned`: the node
is still *there*, fghj just can't currently say whether it matches. Reporting
"this node no longer exists" because a `git` call timed out would be a lie
with a destructive-looking remedy attached.

## The hash, and what it deliberately omits

Drift is detected by hashing, not by field-by-field comparison. When
`start_node` actually launches a container it computes `spec_hash(node, spec)`
and stamps it on the container as an `fghj.config_hash` label — so the
"desired state as of creation" travels with the container itself, surviving
an `fghjd` restart, rather than living in a side table that could disagree
with reality. `config_drift` recomputes the hash from the current graph
(`resolve_node_spec(..., side_effects: false)` — nothing is built, pulled, or
run) and compares.

The rule for what goes *in* is: anything that Docker reads once, at create
time, and never again. `stop_signal` and `stop_grace_period` are the clearest
case — they are stamped onto the container by `create_container` precisely so
that every stop path honours them (see [[docker-and-downloads]]), which means
editing them in `.fghj.yaml` has no effect whatsoever until the container is
recreated. A knob that silently does nothing is exactly what drift detection
is for.

Two categories are excluded from the hash on purpose:

- **Pure identity** — `container_name`, `domain`, `aliases`. These derive
  one-to-one from `node.id` plus the run id (see the projection table in
  [[node-identity-and-domains]]), so they cannot drift independently of the
  rest of the spec. Including them would add no signal.
- **Genuinely ephemeral** — an unpinned port's actual host-side binding,
  which Docker chooses fresh on every start. Hashing it would report every
  single restart as drift, which is worse than reporting none: a signal that
  fires constantly is a signal nobody reads.

The hash is `Sha256`, not `std::hash::DefaultHasher`, for a reason that only
shows up months later: `DefaultHasher` is explicitly documented as unstable
across Rust versions, so upgrading the `fghj` binary would have silently
reported every running container in every workspace as drifted, with no
config having changed at all.

### The stable tag problem, and the three answers to it

Every gap the hash has had comes from one fact: for a node fghj builds, the
image tag is `fghj/<id>:<branch>`, and that string does not change when the
thing it names changes. The tag is stable by design — it is an *address*, not
a version — so `image` alone carries almost no information about what will
actually run.

Three separate pieces of the config had to be added to compensate:

| in the hash | catches | added for |
|---|---|---|
| `build` — `args`, `target`, `secrets`, `ssh` | editing how the image is built | [[build-inputs]] |
| `source.head` — the checkout's commit SHA | committing, pulling, rebasing, switching branch | `AUDIT` B13 |
| `source.dirty` | dirtying a clean checkout | `AUDIT` B13 |

`source` is only included for a node whose image fghj builds
(`build.is_some()`). A node running a published `image:` ignores it
deliberately: a commit in the repo that happens to declare a `postgres:16`
dependency says nothing whatsoever about that container, and hashing it in
would flag every backing service in the workspace as drifted on every commit
— the constant-signal failure again, one table up.

**What the hash still misses.** `dirty` is one bit, so it catches only the
clean → dirty transition. Edit a file in a clean checkout and the node reads
drifted; edit a second file in an already-dirty checkout and it does not.
The hash is still not content-aware — the `Dockerfile`'s own text, and the
files the build copies in, are not read.

That gap is left open on purpose. Closing it means hashing build-context
contents on every drift tick, in every workspace, forever, to serve a case
that already has a better answer: while you are actively editing a file, the
question "has this drifted" is not the one you are asking. You press Restart
(which rebuilds unconditionally) or you commit. Drift detection is for the
change you *forgot* you made — a pull, a rebase, a branch switch, an edit to
a file you are no longer thinking about — and a commit SHA covers all four.

## Where the verdict lives

`config_drift` *reports*; it does not write. It returns `Vec<DriftReport>`,
which `effects::docker::observe::report_config_drift` turns into
`Action::ConfigDriftObserved`, one per container, and the reducer writes it
onto `ContainerObserved::sync`. There is exactly one record of the fact.

That is a deliberate correction. Drift used to be written in two places —
the reducer's copy *and* a separate `synced` field on `RunRegistry`'s own
container record — and the two could, and did, disagree. It is the same
duplication [[state-and-effects]] removed everywhere else; drift was one of
its last instances.

Persistence is lossy in one specific way, and the lossiness is a decision.
SQLite stores `synced` as a nullable bool, so `Orphaned` and `Unknown` both
store as `NULL` and both restore as `Unknown`. The column shape is
deliberately left alone so an older `fghjd` can still read the database, and
re-deriving orphanhood on the next sync tick is both cheap and *more correct*
than trusting a persisted answer: whether a node still exists is a property
of the graph currently on disk, and a verdict from a previous `fghjd`
lifetime is a guess about a file that has since been re-read.

## Related

- [[state-and-effects]] — why the observation is an `Action` and not a write.
- [[run-lifecycle-and-registry]] — the other reconciliation loop, the one
  that compares running against recorded, and `ensure_running`, the one path
  that acts on a drift verdict.
- [[docker-and-downloads]] — the graceful `stop_and_remove` that makes
  recreating a drifted container on an ordinary action safe.
- [[branch-ownership-model]] — why the graph changes under a live run.
- [[build-inputs]] — the `#Build` block the hash now covers.

## Status

Implemented: `src/runs/spec.rs` (`spec_hash`, `NodeSpec`),
`src/runs/observe.rs` (`config_drift`, `DriftReport`),
`src/runs/start_node.rs` (the `fghj.config_hash` label),
`src/state/sync_status.rs` (the enum and its SQLite round trip),
`src/effects/docker/observe.rs` (`report_config_drift`), surfaced in the UI
as a pill on the graph node (`GraphView.svelte`) and in the drawer
(`Drawer.svelte`) — with `Unknown` rendering nothing, since it is the one
verdict with nothing to say.

Acted on in exactly one place: `runs::orchestrate::top_up_may_skip`, which
`ensure_running` consults per node. No background loop acts on a verdict.
