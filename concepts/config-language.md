# The config language: CUE, serde, and how `.fghj.yaml` evolves

## Two schemas for one file

`.fghj.yaml` has two descriptions of what it may contain, and they are not
the same artifact:

- **`schema/*.cue`** — the CUE schemas. Readable, heavily commented,
  expressive enough to state constraints a Rust type can't ("at most one
  `primary` port", "a host alias may never end in `fghj.internal`"). They
  are checked by `fghj validate`, an opt-in CLI command that shells out to
  an external `cue` binary the user may not have installed.
- **`src/resolver/config.rs` and friends** — the serde types the daemon
  actually deserializes into.

The daemon never runs `cue`. It cannot: requiring a second binary at runtime
to read a config file would make the tool fail on machines where it is not
installed, which is most of them.

So the CUE is an **authoring aid** — for the person writing the file, for
their editor, for an agent generating one, for CI — and the Rust types are
the **enforcing boundary**. Every claim the folder makes about what a
`.fghj.yaml` can contain must ultimately be true of the Rust types, because
that is the only place the bytes are actually checked.

## The obligation that runs the other way

Given that split, the CUE has one hard obligation: **it must never accept
something the daemon would reject**, and — established later, the hard way —
**it must never reject something the daemon accepts** either.

The first direction is the dangerous one: `fghj validate` says "ok" and the
daemon then refuses the file, which is the worst possible outcome for a
validation tool. It teaches people not to trust it.

The second direction was originally written off here as harmless ("the
author gets a preview of a constraint that would have been fine"). It
isn't. `#Flow.dependencies` carried `& [_, ...]` — non-empty — while the
Rust `FlowConfig` accepted `dependencies: []` without comment. So a legal,
working flow failed `fghj validate`, and because the schema was treated as
the spec, the reference documentation then went and wrote the phantom rule
down as fact ("a flow's `dependencies` list must be non-empty"). A schema
that is too strict doesn't stay a small annoyance; it invents language
rules, and the docs launder them into real ones. Same lesson as
[[AUDIT]] §4.4: fix the boundary, not the prose that agrees with it.

### Why this class of bug recurs: CUE presence is not what it looks like

Three mismatches of exactly this kind have been found (`#Service.dependencies`
missing its `| *[]`, `#ComponentConfig.services` not being `!`,
`#Flow.dependencies` being non-empty), which is enough to be a pattern
rather than three accidents. The pattern comes from a mechanical detail that
is easy to read past: **`fghj validate` runs `cue vet` without `-c`, so a
plain `field: T` is only enforced as *present* when `T` is not already
concrete.**

Concretely:

- `version: string & =~"…"` — a missing `version` leaves a non-concrete
  string, so `vet` errors. Effectively required. Matches serde.
- `services: [Name=…]: #Service` — a missing `services` leaves an empty
  struct, which *is* concrete, so `vet` passes. Effectively optional —
  while serde required it. That was the bug; `services!:` fixes it, because
  `!` is enforced regardless of concreteness.
- `dependencies: [...#Dependency]` — a missing list is non-concrete, so
  required; but `& [_, ...]` additionally rejects the empty list, which
  serde allows.

So the reliable rule when adding a field: decide what serde does first, then
spell it in CUE as `field!:` (serde requires it), `field?:` (serde has
`Option` with no `#[serde(default)]`… which still defaults to `None`, so
prefer `?` only for genuinely optional), or `field: T | *default` (serde has
`#[serde(default)]`). Never rely on a plain `field: T` to mean "required" —
it only does so by accident of the type.

`resolver::name`'s `rust_and_cue_agree_on_what_a_name_is` enforces the
obligation mechanically. It reads `schema/*.cue`, pulls out every `=~"…"`
identifier constraint, and fails if any of them differs from `NAME_PATTERN`.
The pattern is kept as a string constant purely so this comparison can be
verbatim. It is a checked fact rather than a comment asking people to
remember, which is the difference between an invariant and an aspiration.

## What the type system is for: `Name`

The audit's framing (B8) was that `schema/*.cue` is a linter, not a type
system. The concrete consequence: a service named `"My Service!"` parsed
fine, flowed unsanitized into `node.id`, and from there into a DNS name and
a Docker network alias that are simply invalid — while the *container* name,
which goes through `sanitize_label`, worked. One unvalidated input reaching
three namespaces with three different escaping disciplines.

`resolver::name::Name` is where that stops. It is a newtype whose
`Deserialize` impl enforces `^[a-z0-9][a-z0-9-]*$`, applied to every field
that reaches an id: service map keys, flow `service`, backing `name`, a
`kind: service` dependency's `services:` list, shared-backing
`service`/`name`, port `name`, volume `name`. There is no `From<&str>` — the
only constructors are `Deserialize` and `Name::parse`, both validating — so
*holding* a `Name` is proof it is safe in every downstream namespace
unescaped.

The alphabet is the **intersection** of what a DNS label, a Docker network
alias and a container name accept, not the union. That choice is what makes
the guarantee cheap: nothing downstream needs to escape, so no two escapings
can disagree. See the projection table in [[node-identity-and-domains]] for
the eight namespaces this protects.

The check is hand-written rather than a regex because a bespoke check can say
*why* a name was rejected, which a regex mismatch cannot.

## Where each kind of check lives

| Check | Where | Why there |
|---|---|---|
| Field shapes, required/optional, defaults | serde types | The only boundary the bytes actually cross. |
| Identifier alphabet | `Name::deserialize` | Must hold before the value reaches an id; a later check would run after the damage. |
| `version` compatibility | `Version::deserialize` | Same — a file from an incompatible major must not deserialize at all. |
| Cross-field consistency within a node (one `primary` port, pointless `wildcard`) | `resolver::validate`, as warnings | Needs the whole `ports` map, so it can't be a field-level check. |
| Cross-node facts (name collisions, dangling references, cycles) | `resolver::uniqueness`, `resolver::cycles`, as warnings | Needs the whole resolved graph, and one node's config is not wrong on its own. |
| Everything expressible but unenforceable | `schema/*.cue` prose and constraints | For the author, before they ever run the daemon. |

The pattern: the further a check is from the byte boundary, the more likely
it is a *warning* rather than a rejection — because by then the thing being
reported is a relationship between independently-valid configs, and refusing
to parse a file over it would blame the wrong repo. See
[[failure-semantics]].

## `version:`, and why minor is not a barrier

The original CUE pinned `version` to the literal `"1.0"`, and serde never
checked it at all — so it was decoration. Worse, the pin was incoherent with
the design: no repo owns the config, every repo ships its own `.fghj.yaml`,
and they all resolve into one graph *together*. If adopting a new schema
version required every repo to change at once, the language could never
evolve without reintroducing exactly the central-coordination problem
[[flat-workspace-model]] exists to abolish.

So the rule is: **major is a compatibility barrier, minor is not.**

- A **different major** is rejected outright. It means the file says
  something this build cannot correctly interpret, and guessing would be
  worse than refusing.
- **Any minor** is accepted, including one newer than this build knows. That
  is what makes staggered adoption work: a repo can start using a 1.1 feature
  while its peers stay on 1.0, and they still resolve together.

Accepting a newer minor *silently* would be its own failure — the features
added after this build's minor are simply not there, and the symptom would be
config that reads as though it were honoured. So `scan_workspace` pairs
acceptance with an advisory warning naming the repo and both versions.

`SCHEMA_VERSION` is the contract for changes: bump minor when adding a
backwards-compatible field, bump major only for a change that would make an
older file *mean something different*. Adding a field is always minor.
Changing what an existing field does is always major, even when the type is
unchanged.

## How to add to the language

The order matters, because the drift test enforces one direction:

1. Decide whether the new thing is expressible as an addition (minor) or
   changes an existing meaning (major).
2. Add the field to the CUE with a doc comment explaining *why* it exists and
   what it rules out — the CUE is where authors read about the language.
3. Add it to the serde type with `#[serde(default)]` if it's optional, so
   every existing file keeps its current meaning byte-for-byte.
4. Carry it into the graph node, then into whatever consumes it.
5. If it affects how a container runs, add it to `spec_hash` — otherwise
   editing it is invisible to drift detection ([[config-drift]], and see
   [[build-inputs]] for the time that was missed).
6. Write a test that a config *without* the field still resolves to what it
   used to mean. That is the real backwards-compatibility check; the version
   number is only a label for it.
7. Bump `SCHEMA_VERSION.minor`.

## Related

- [[node-identity-and-domains]] — what `Name` protects, namespace by
  namespace.
- [[failure-semantics]] — blocking vs. advisory, and why most cross-node
  checks are warnings.
- [[flat-workspace-model]] — why the language has to evolve without
  coordination.
- [[config-drift]] — why a new field usually belongs in `spec_hash`.

## Status

Implemented: `schema/component.cue`, `schema/dependency.cue` (the authoring
schemas), `src/resolver/config.rs` and its siblings (the enforcing types),
`src/resolver/name.rs` (`Name`, `NAME_PATTERN`, the CUE drift test),
`src/resolver/version.rs` (`Version`, `SCHEMA_VERSION`, the major/minor
rule), `src/resolver/validate.rs` and `uniqueness.rs` (graph-level checks),
`fghj validate` in `src/main.rs` (the opt-in `cue` shell-out, with the
schemas baked in via `include_str!`).
