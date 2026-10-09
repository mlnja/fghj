# Building someone else's code: git build contexts

Sometimes you want to run a project that ships no Dockerfile, or no
`.fghj.yaml`, or one that doesn't fit your setup. An open-source
microservice, say. You can't edit its repo. You still want to say how to
build it, how to run it, and which of your flows it belongs to.

`include:` doesn't fit. It means "use *their* fghj definition": their
services, their flows, their decisions. Here the definition is yours, and
only the code is theirs.

## The config

The same shape Compose uses: a git URL as the build context.

```yaml
services:
  geocoder:
    build:
      context: https://github.com/acme/geocoder.git#v2.3.1:server
      dockerfile_inline: |
        FROM golang:1.23 AS build
        WORKDIR /src
        COPY . .
        RUN go build -o /geocoder ./cmd/geocoder
        FROM gcr.io/distroless/base
        COPY --from=build /geocoder /geocoder
        ENTRYPOINT ["/geocoder"]
    ports:
      http: {container: 8080, primary: true}
    depends_on:
      db: {condition: service_healthy}
  db:
    image: postgres:16
```

- **`context: <url>#<ref>:<subdir>`.** `ref` is a branch, tag or commit and
  is optional; without it, fghj uses the remote's default branch. `subdir` is
  optional too. A context counts as remote when it starts with `https://`,
  `http://`, `ssh://`, `git@` or `file://`.
- **`dockerfile_inline`** is the Dockerfile itself, because the repo doesn't
  have one. `dockerfile:` still works, as a path inside the context, for a
  repo that has a Dockerfile you'd rather not use as-is. Setting both is a
  blocking error.
- Everything else (`ports`, `environment`, `command`, `depends_on`,
  `healthcheck`, flows) is an ordinary service of **your** repo.

## Identity

The service belongs to the repo that declares it. Its id is
`geocoder.myrepo`, and its domain, volumes and run membership follow from
that, like any other service of yours. Other repos reach it through your
flows. They can't name the source repo, which isn't a repo in the
workspace.

## Where the code goes

fghj clones the source into `<workspace>/.fghj/sources/<name>@<ref>`, or
`<name>` without a ref. `<name>` is the last segment of the URL, and a `/`
in the ref becomes `-`. Several services that build from the same URL and ref
share one clone. Two different URLs that would land in the same folder
(`acme/geocoder#main` and `fork/geocoder#main`) are a blocking warning.

- **Not a workspace repo.** The scan only reads top-level folders, so the
  clone is never read as a repo. A `.fghj.yaml` inside it means nothing.
- **Pulled like a repo.** "Pull all", a flow's pull, and a node's download
  clone the sources of the nodes they would start. The clone runs as the
  workspace owner, like every other clone.
- **A commit ref is checked out.** `git clone --branch` takes only branches
  and tags, so a commit-looking ref (7–40 hex digits) is cloned whole and
  then checked out detached. A ref the remote lacks fails the pull and leaves
  no clone behind.
- **Never updated behind your back.** An existing clone isn't fetched again.
  Pin a tag or a commit if you want a fixed version. To move to a new
  version, change the ref, which gives a new folder.
- **Starting without it is an error.** If the source isn't cloned, starting
  the service fails with "source not pulled", the same way a not-yet-pulled
  repo can't start. Start never clones.

## Building

The build context is the clone, plus `subdir`. An inline Dockerfile is added
to the build tar under a reserved name, `.fghj.Dockerfile`, so nothing is
written into the clone and it stays clean. The clone's `.git` is always
left out of the build context, whatever its `.dockerignore` says, as BuildKit
does for a git URL context. The image tag is `fghj/<id>:<ref>`, or
`fghj/<id>:default` without a ref.

The clone's HEAD is part of the node's spec hash, in place of the declaring
repo's (`Node::build_checkout`), which also feeds the container's
`fghj.source_*` labels and the Drawer's "built from". If
you check out something else in the clone, the service shows as drifted. The
UI shows the clone's HEAD and whether it has local changes: it's a normal
checkout, so you can patch the code there while debugging.

## Status

Implemented: `schema/component.cue` (`#BuildFull.dockerfile_inline`, a
context may be a git URL); `resolver::build_source` (parsing, clone path,
on-disk state) and `BuildSource` on `NodeBuild`; the checks in
`resolver::validate` (`check_build`, `check_source_paths`); cloning in
`downloads.rs` (`clone_source_logged`, from Pull all, a flow's pull and a
node's download); the build in `runs/node_spec.rs` and `docker.rs`
(`INLINE_DOCKERFILE`, `skip_git_dir`); the Drawer's source rows and Pull
button.
