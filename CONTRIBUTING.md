# Contributing to fghj

Thanks for looking. fghj is early, so the most useful contributions right now
are bug reports from real multi-repo setups, missing-feature reports ("I
couldn't express X in `.fghj.yaml`"), and docs fixes.

## Reporting a bug

Open a [bug report](https://github.com/mlnja/fghj/issues/new?template=bug_report.yml).
The output of `fghj doctor` and `fghj --version` answers most first
questions, so please include both. If the bug is about how a graph resolves,
the smallest `.fghj.yaml` files that reproduce it help more than anything else.

Security problems go through [SECURITY.md](SECURITY.md), not public issues.

## Before you write code

For anything bigger than a small fix, open an issue first so we can agree on
the shape before you spend time on it.

Check `concepts/` for a file that already explains why the current behaviour
is what it is. It's the durable design record, one file per subsystem, and
`concepts/AUDIT.md` is the punch list of known gaps. When you settle something
new, fold the decision into the relevant concept file rather than leaving it
only in a commit message.

## Building

You need a [supported OS](https://fghj.mlops.ninja/getting-started/installation/#prerequisites),
Rust (stable), Node 22+ and Docker. `cue` is optional and only needed for
`fghj validate` and the schema check.

```bash
# The Svelte UI is embedded into the daemon at compile time: build it first
cd ui && npm install && npm run build && cd ..

cargo build
```

`ui/dist` is gitignored, and `include_dir!("$CARGO_MANIFEST_DIR/ui/dist")` is
a proc macro that panics at compile time if it's missing. If a fresh clone
won't compile, that's almost always why.

## Checks

These are what CI runs, so run them before you push:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cue vet schema/*.cue   # if you touched schema/
```

CI also does a no-push build of `sidecar/Dockerfile`. The tests that need a
real Docker daemon and a real root `fghjd` can't run in CI at all, so if your
change touches the daemon, DNS, the CA or networking, run it against a real
`fghjd` locally and say what you tried in the PR.

## Docs

The docs site is the Astro Starlight project in `docs/`. Every page has an
"Edit page" link on [fghj.mlops.ninja](https://fghj.mlops.ninja/), and pull
requests get a Netlify preview.

```bash
cd docs && npm install && npm run dev
```

## Pull requests

- Keep each PR to one change.
- If you changed `.fghj.yaml` semantics, update `schema/*.cue`, the reference
  page `docs/src/content/docs/reference/fghj-yaml.md`, and the relevant
  concept file in the same PR.
- Write the commit message as the *why*: the diff already says what.

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). By
taking part you agree to it.
