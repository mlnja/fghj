# Builder parity: build where `docker build` would have

> Implemented in `src/docker/cli_build.rs` (`BuildxCli`);
> `src/platform/` (`Session::as_user_in_session`); `src/buildx.rs`
> (`default_builder`, `HostBuilder`); `src/docker/image.rs`
> (`BuildOpts::{cli, builder}`, `solve_on_host_builder`, `buildkit_image_id_of`,
> `retire_builder_if_image_changed`, `prepare_fghj_buildkit`, `load_image_tar`,
> `FGHJ_BUILDKIT_CONTAINER`); `src/orchestrator/node_spec.rs` (`build_node_image`).

## The bug this closes

A user built a repo with `docker build` and it succeeded. `fghj` built the
same repo, from the same checkout, and it failed. Nothing in the repo
explained the difference, and the obvious conclusion — that `fghj` was
mangling the build — was wrong.

`docker build` does not necessarily use the BuildKit inside `dockerd`. When
`buildx` has a container-backed builder selected — OrbStack and Docker Desktop
both create one, and `docker buildx ls` marks it with a `*` — the CLI sends
the build to *that container*, which carries its own BuildKit release. On the
machine where this was diagnosed:

```
orb-builder *  docker-container   BuildKit v0.32.2
default        docker             BuildKit v0.29.0
```

`fghj` hardcoded the `Moby` driver, which means `dockerd`'s embedded BuildKit:
the second row. Three minor versions apart, and far enough apart to disagree
about whether a `RUN` may redirect output into a directory that does not exist
yet. The user's actual Dockerfile needed `mkdir -p /out` under v0.29.0 and not
under v0.32.2.

The failure mode is what makes this worth a document. Both engines are
correct, both are current, neither logs its own version, and the error
surfaces as a build failure attributable to anything. A user cannot
reasonably be expected to discover that their CLI and their tooling are
talking to two different build engines.

## The precept

**Build where the user's own `docker build` would have built.** Agreement
with the CLI on the same machine is worth more than any property fghj could
choose for itself, because disagreement is unattributable: the user has no
way to tell a bug in their Dockerfile from a bug in fghj.

## Resolution: the owner's own `docker buildx build`

The one thing guaranteed to build where `docker build` would is the user's
`docker` CLI itself. So that is what fghj runs, as the workspace owner:

```
docker buildx build --progress=plain --load -t <tag> -f <dockerfile> \
  [--target …] [--platform …] [--build-arg …] [--secret id=…,src=…] [--ssh default=…] -
```

- **Same builder by construction.** buildx picks it, from the owner's
  `~/.docker`; fghj doesn't need to know which one it is.
- **Their registry logins.** A private `FROM`, or a BuildKit image that came
  through a company mirror, needs the owner's credential helper
  (`credsStore: osxkeychain`, `ecr-login`, `secretservice`, …). Root `fghjd`
  has none, and a credential store only answers inside the owner's login
  session. As root, `Session::as_user_in_session` wraps the CLI per OS, always
  ending in `env -i` so root's environment stays behind:
  - **macOS:** `launchctl asuser <uid> sudo -u #<uid> -H env -i … docker …`.
    Both halves are needed: `asuser` for the session the keychain answers in,
    `sudo` for the uid. `asuser` needs root, so the drop has to come after it.
  - **Linux:** `setpriv --reuid --regid --init-groups env -i … docker …`.
    `setpriv` is util-linux, so it's always there, where `sudo` may not be.
    `--init-groups` keeps the owner's `docker` group, without which the CLI
    can't reach the socket. When the owner is logged in, `XDG_RUNTIME_DIR` and
    `DBUS_SESSION_BUS_ADDRESS` point at their session bus, which
    `secretservice` and `pass` need.
  - **Anywhere else:** no CLI, so the bollard fallback.
- **Same cache** as their own builds, and `--load` puts the image in dockerd.
- **fghj's context, not the directory.** The context goes in on stdin as the
  same tar the bollard path sends (`-` as the context, `-f` naming a file
  inside it). A git source's `.git` and an inline Dockerfile are therefore
  handled exactly as before, and the Events tab still says how big the context is.
- **The failing step's output comes back.** The error carries the last 40
  lines of `--progress=plain` output.
- **Cancellation.** The CLI runs in its own process group. Dropping the build
  sends that group SIGTERM, which buildx turns into a cancelled solve.
- **Hints and plugin hooks are off** (`DOCKER_CLI_HINTS=false`,
  `DOCKER_CLI_HOOKS=false`). A `docker scout` hook on every build is for a
  person at a terminal.
- **The image must land where fghjd looks.** After the build fghj inspects the
  tag through its own Docker connection. If it isn't there, the CLI's context
  and the socket `fghj wire` gave fghjd point at different engines, and the
  error says so.

`fghjd` runs under launchd or systemd with a bare `PATH`, so
`BuildxCli::find` looks for `docker` in the usual places itself
(`/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/snap/bin`,
`~/.orbstack/bin`, `~/.docker/bin`, Docker.app, OrbStack.app), then checks
`docker buildx version` as the owner. The CLI's `PATH` starts with the
directory the binary is in, where OrbStack and Docker Desktop put their
`docker-credential-*` helpers.

## Fallback: bollard, only when there is no CLI

When `find` finds no `docker` binary, no buildx plugin, no owner to run as,
or no way to run as them on this OS,
the build goes through bollard as before. A *failed* CLI build is not retried
there: building twice would only hide the real error.

The fallback still follows buildx's selected builder. It reads buildx's plain
JSON under the **owner's** `~/.docker/buildx` (`current`, then
`instances/<name>`). A `docker-container` driver resolves to the container
`buildx_buildkit_<nodeName>`. That is buildx's fixed convention, hardcoded in
its `driver/docker-container/factory.go`, not something stored in the
instance file. Any other driver means dockerd's embedded BuildKit. The home
must be the owner's: root's `$HOME` holds no buildx config, and reading it
would resolve to "no builder" on every machine.

### fghj runs its own BuildKit, at buildx's image

bollard can't talk to buildx's container. It stamps ownership labels on what
it manages and refuses one it didn't create:

```
ownership label `com.github.fussybeaver.bollard.buildkit.managed` does not match
```

So the fallback runs a persistent container of its own, `fghj_buildkit`, with
a `fghj_buildkit_state` cache volume, on the **same image** as buildx's.

### Pin by local image id, and never pull

`buildkit_image_id_of` takes the image id straight from buildx's container.
Both alternatives fail:

- **A tag moves.** `moby/buildkit:buildx-stable-1` pulled today is a
  different BuildKit from the one buildx's container was made from a week ago.
- **A repo digest names a registry.** The first `RepoDigests` entry is
  whatever the image was pulled through, for instance
  `<account>.dkr.ecr.<region>.amazonaws.com/dockerhub/moby/buildkit@sha256:…`.

bollard's `bootstrap()` pulls the image before creating a container, every
time, with no credentials (`// TODO: registry auth` in its source). A mirror
needing a login then fails with "no basic auth credentials", though the image
is already on the machine. Because buildx's container exists, its image is
always local, so `prepare_fghj_buildkit` creates `fghj_buildkit` from that id
itself, before `bootstrap()` runs:

- with bollard's ownership labels and the resource id of the existing volume;
- with bollard's host config: privileged, init, `host` network, the state
  volume at `/var/lib/buildkit`, restart `unless-stopped`;
- with its daemon argument `--allow-insecure-entitlement=network.host`.

`bootstrap()` then finds a container it recognises and only starts it. This
mirrors `DockerContainer::create` at the pinned bollard rev. The `#[ignore]`d
build tests catch any drift: `bootstrap()` rejects a container that differs.

`retire_builder_if_image_changed` removes `fghj_buildkit` when its image id no
longer matches buildx's, and keeps the cache volume (`v: false`). Containers
made by older fghj versions, pinned by digest, are retired once this way.

Two costs come with the fallback:

- **A second build cache.** The first fghj build of a repo is cold.
- **Every image crosses the socket twice.** BuildKit in a container can't
  write to dockerd's image store, so the solve exports a tarball and
  `load_image_tar` streams it back.

## Which engine ran is reported, always

The Events tab names it. "Which BuildKit built this" is the first thing worth
knowing when a build behaves differently from the CLI, and no error message
mentions it:

```
building image   fghj/app:main  ·  /Users/me/src/app  ·  buildkit: orb-builder (docker buildx)
```

The tag is `docker buildx` when buildx chose a builder fghj doesn't read
(dockerd's own, a remote). On the fallback it is the builder's name, or
`dockerd` for the embedded path.

## Status

Implemented. Verified against a real daemon by the `#[ignore]`d tests in
`src/docker/image.rs` (`mod build_tests`):

- the CLI route builds fghj's tar with an inline Dockerfile, a build arg and a
  secret, and leaves `.git` out;
- a failing step's own output reaches the error;
- the fallback's container is made from buildx's local image id, with no pull;
- the fallback's container is reused across builds and runs buildx's BuildKit
  version, while buildx's own container is left alone;
- `--build-arg` and `--target` survive the fallback.

The command lines the root route builds are unit-tested per OS. Whether a
credential store actually answers there (the keychain under `launchctl asuser`,
the session bus under `setpriv`) needs a real `fghjd`, and no test covers it.
