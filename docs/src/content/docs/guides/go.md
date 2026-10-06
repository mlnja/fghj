---
title: "Guide: Go services in fghj"
description: A Go service in fghj from scratch — the multi-stage build, a healthcheck in an image with no shell, private modules over ssh, graceful shutdown, live reload, and Delve.
---

A Go service in fghj is an ordinary multi-stage Docker build. Four things
are worth knowing before you write one:

- **`ListenAndServe` on `":8080"`, not `"127.0.0.1:8080"`** — the one-line
  mistake that makes a published port connect and then hang.
- **A healthcheck needs a binary, not a shell**, because the images you
  want for Go don't have one.
- **`RUN --mount=type=cache` is worth using** — fghj builds everything
  through BuildKit, and a Go rebuild is where that pays off most.
- **SIGTERM kills a Go process outright** unless you handle it.

## A service, end to end

```docker title="Dockerfile"
FROM golang:1.25 AS build
WORKDIR /src

# Module download in its own layer, before the source: editing a .go file
# then doesn't re-download the module graph.
COPY go.mod go.sum ./
RUN go mod download

COPY . .
# CGO_ENABLED=0 produces a static binary, which is what lets the final stage
# be an image with no libc at all.
RUN CGO_ENABLED=0 go build -o /out/app ./cmd/app

FROM gcr.io/distroless/static:nonroot
COPY --from=build /out/app /app
ENTRYPOINT ["/app"]
```

```yaml title=".fghj.yaml"
version: "1.0"

services:
  api:
    build:
      context: .
    ports:
      "8080":
        primary: true
    environment:
      OWN_URL: https://${FGHJ_SERVICE_FQDN_HTTP}
    healthcheck:
      test: ["CMD", "/app", "-healthcheck"]
      interval: 2
      retries: 15
    stop_grace_period: 15
```

### Listen on every interface

```go
// Right: every interface in the container's namespace.
log.Fatal(http.ListenAndServe(":"+port, nil))

// Wrong: only the container's own loopback. The published port accepts the
// connection and then nothing answers it.
log.Fatal(http.ListenAndServe("127.0.0.1:"+port, nil))
```

Go's standard library has no buffering surprise to go with it: `log` and
`fmt.Print` write straight to the file descriptor, so there is no
`PYTHONUNBUFFERED` equivalent to remember.

### A healthcheck in an image with no shell

`distroless/static` (and `scratch`) have no `curl`, no `wget`, and no `sh`.
Docker's exec-form `CMD` doesn't need a shell, so the clean answer is to
make your own binary able to check itself:

```go
func main() {
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}

	// Must come before anything that binds a port or opens a database: this
	// invocation is a short-lived second process inside the same container,
	// not the server.
	if len(os.Args) > 1 && os.Args[1] == "-healthcheck" {
		resp, err := http.Get("http://localhost:" + port + "/health")
		if err != nil || resp.StatusCode != http.StatusOK {
			os.Exit(1)
		}
		return
	}

	// ... the actual server
}
```

```yaml
healthcheck:
  test: ["CMD", "/app", "-healthcheck"]
```

A healthcheck runs the command directly — it does not go through the
image's `ENTRYPOINT` — so this stays the raw binary even when the
entrypoint is something else (which it will be, once you add Delve below).

If you'd rather not put that branch in your program, use `alpine` as the
final stage and poll with busybox wget:

```yaml
healthcheck:
  test: ["CMD", "wget", "-q", "-O-", "http://localhost:8080/health"]
```

### Build caching: layers and cache mounts

fghj builds every image through **BuildKit**, with no classic-builder
fallback, so cache mounts are available to you unconditionally. Go gets more
out of them than most languages, because `go build` keeps a compiled-package
cache that is otherwise thrown away with the build stage every time:

```docker
RUN --mount=type=cache,target=/root/.cache/go-build \
    --mount=type=cache,target=/go/pkg/mod \
    CGO_ENABLED=0 go build -o /out/server ./cmd/server
```

That survives across builds, which is the difference between recompiling your
dependencies on every change and recompiling only what you touched.

Keep the `COPY go.mod go.sum` / `go mod download` split as well. The two
mechanisms cover different failures: the layer split means an edit to your
own code doesn't re-download modules at all, while the cache mount means a
change to `go.mod` — which does invalidate that layer — still doesn't start
from an empty cache.

If a build fails with `the --mount option requires BuildKit`, your Docker
daemon is too old (BuildKit needs Engine 18.09+, and has been the default
since 23.0). `fghj doctor` reports that directly.

### Private modules

Go's module proxy fetches over HTTPS, so a private repo needs a credential
at build time. fghj can forward **your** ssh-agent into the build:

```yaml
    build:
      context: .
      ssh: true
```

```docker
RUN --mount=type=ssh \
    git config --global url."git@github.com:".insteadOf "https://github.com/" && \
    GOPRIVATE=github.com/acme/* go mod download
```

The agent is the workspace owner's, the same one fghj already lends to `git
clone` — a repo's config can ask for the socket, never for a key.

### Graceful shutdown

Go's runtime does nothing special with SIGTERM: the process dies where it
stands, mid-request. Handle it, and give fghj's stop a window wide enough
to finish:

```go
srv := &http.Server{Addr: ":" + port}
go func() {
	if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
		log.Fatal(err)
	}
}()

ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
defer stop()
<-ctx.Done()

shutdownCtx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
defer cancel()
_ = srv.Shutdown(shutdownCtx)
```

```yaml
    stop_grace_period: 15
```

`stop_grace_period` is how long Docker waits after the stop signal before
`SIGKILL`. Keep it comfortably above your own shutdown timeout — the
default is 10 seconds, matching Docker's.

## Migrations as a task

A migration is a container that's *supposed* to exit, which is its own node
kind. With no `image:`, a task runs the owning service's **own built
image** with a different command — so a `migrate` subcommand in your
binary needs nothing extra built:

```yaml
      - kind: task
        name: migrate
        command: ["/app", "migrate"]
        after: ["db"]
        environment:
          DATABASE_URL: postgres://app:dev@${FGHJ_SERVICE_FQDN:db}:5432/app
```

A task isn't started until it has *finished*, and a non-zero exit fails the
node and blocks everything downstream — so a failed migration stops the
service coming up against an unmigrated database rather than racing it. The
default `run: on_start` re-runs it on every start and top-up, so the command
must be idempotent.

## Live reload

Go has no in-process reloader, so a dev loop means rebuilding inside the
container. Put that in its own stage and point `build.target` at it:

```docker
FROM golang:1.25 AS dev
WORKDIR /src
RUN go install github.com/air-verse/air@latest
COPY go.mod go.sum ./
RUN go mod download
CMD ["air"]
```

```yaml
    build:
      context: .
      target: dev
    volumes:
      - host: .
        container: /src
```

`target` is plain `docker build --target`.
Mounting your checkout at `/src` is what makes `air` see your edits; because
the mount shadows everything the stage copied there, keep the module cache
out of the mounted tree (it's at `/go/pkg/mod`, which is why the `COPY`/`go
mod download` above still earns its place).

## Debugging

Go is the one runtime where the debugger has to be the **launcher**: Delve
starts your binary rather than attaching to a running one. That makes the
debug image genuinely a different image, which is a good reason to make it
a different stage:

```docker
FROM golang:1.25 AS debug-build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
# -N -l disables inlining and optimisation. Without it, stepping jumps
# around and half your variables read as "optimized out". Never pair this
# with -ldflags="-s -w" — that strips the DWARF info Delve needs.
RUN CGO_ENABLED=0 go build -gcflags="all=-N -l" -o /out/app ./cmd/app
RUN go install github.com/go-delve/delve/cmd/dlv@latest

# debian-slim rather than distroless: the entrypoint below is a shell
# script, and distroless has no shell.
FROM debian:bookworm-slim AS debug
COPY --from=debug-build /out/app /app
COPY --from=debug-build /go/bin/dlv /usr/local/bin/dlv
COPY docker-entrypoint.sh /usr/local/bin/
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["/app"]
```

```bash title="docker-entrypoint.sh"
#!/bin/sh
set -e
# Peel the binary off the front of CMD so what's left is its own arguments.
# "${@:1}" is a bashism; this is the POSIX way, and /bin/sh here is dash.
bin="$1"
shift
cont="--continue"
[ -n "$FGHJ_DEBUG_WAIT" ] && cont=""
exec dlv exec "$bin" \
  --headless --listen=0.0.0.0:2345 \
  --api-version=2 --accept-multiclient $cont -- "$@"
```

```yaml
    build:
      context: .
      target: debug
    debug: 2345
```

Make the script executable before you build it — `chmod +x
docker-entrypoint.sh`. Docker copies the mode from disk, and a
non-executable entrypoint fails the container with `permission denied`
rather than anything that mentions the script.

`--continue` is why the service still starts normally: Delve listens, and
the program runs at full speed, because a breakpoint doesn't exist until an
IDE has attached and sent one. Dropping `--continue` is what
`FGHJ_DEBUG_WAIT` asks for — the per-container switch in the UI, for the one
case attaching can't reach: code that runs during startup, including
package-level `var` initialisation. `--accept-multiclient` lets you detach
and reattach without killing the process.

Attach from VS Code, or with Delve's own client:

```bash
dlv connect api.myrepo.myworkspace.fghj.raw.internal:2345
```

```json title=".vscode/launch.json"
{
  "name": "api (fghj)",
  "type": "go",
  "request": "attach",
  "mode": "remote",
  "host": "api.myrepo.myworkspace.fghj.raw.internal",
  "port": 2345,
  "substitutePath": [{ "from": "${workspaceFolder}", "to": "/src" }]
}
```

`substitutePath` matters: the DWARF info records the paths the *build stage*
saw (`/src/...`), and your breakpoints are on paths in your checkout.
Getting it wrong is the usual cause of a breakpoint that's set and never
hit — Delve is attached and simply disagrees about which file you meant.

[Attaching a debugger](/guides/debugging/) has the whole contract, and
[tutorial chapter 7](/tutorial/07-attaching-a-debugger/) walks through
catching Go startup code with the halt switch.

## Trusting fghj's CA from a Go container

Service-to-service calls should use the raw zone
(`${FGHJ_SERVICE_FQDN:name}`) and need nothing. If you specifically need the
proxied HTTPS name from inside the container, the container has to trust
fghj's CA — and `distroless` has no `update-ca-certificates`. Go reads
`SSL_CERT_FILE`, so point it at a bundle you copy in:

```docker
COPY ca-cert.pem /etc/ssl/fghj-ca.pem
ENV SSL_CERT_FILE=/etc/ssl/fghj-ca.pem
```

The certificate is at `/private/var/lib/fghjd/ca/ca-cert.pem`. Note that
`SSL_CERT_FILE` *replaces* the default roots rather than adding to them, so
concatenate it with the image's own bundle if the service also calls the
public internet.

## Gotchas, collected

| Symptom | Cause |
|---|---|
| Port connects, nothing answers | Listening on `127.0.0.1`. Use `":"+port`. |
| `the --mount option requires BuildKit` | Docker daemon too old for BuildKit. Run `fghj doctor`. |
| Healthcheck always fails | No shell or `curl` in the final image. Use a `-healthcheck` flag or `alpine` + `wget`. |
| Container dies mid-request on stop | No SIGTERM handler. |
| `permission denied` on start | `chmod +x docker-entrypoint.sh`. |
| Breakpoints never hit | Missing `-gcflags="all=-N -l"`, or a wrong `substitutePath`. |
| `x509: certificate signed by unknown authority` | Calling a `*.fghj.internal` name without fghj's CA. Use the raw zone, or set `SSL_CERT_FILE`. |
