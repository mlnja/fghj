---
title: "Guide: attaching a debugger to a container"
description: Declare debug:<port> in .fghj.yaml, start the debugger in your own Dockerfile, and attach your IDE — with a copy-paste recipe per runtime.
---

fghj gives you a stable address for a debug port, and one switch for the one
thing your image cannot work out for itself. Starting the debugger is your
image's job. That split is deliberate: fghj never touches your `Dockerfile`,
never derives a second image from it, and has no idea which language or
runtime is inside. An adapter table of per-runtime incantations in the
daemon would be wrong for every image that wraps its entrypoint in a shell
script, and would make fghj responsible for guessing what is inside an image
it did not build.

## The whole contract

Declare the port your debugger listens on *inside* the container:

```yaml title=".fghj.yaml"
version: "1.0"
services:
  api:
    build:
      context: .
    ports:
      "3000":
        primary: true
    debug: 9229
```

That does **one** thing: it publishes the port, so it answers at
`{node}.{workspace}.fghj.raw.internal:9229` — on the number you declared,
from your host *and* from inside the run's network. See
[HTTP vs. raw](/guides/networking-http-vs-raw/) for what that address is.

Nothing is injected into the container's environment. `debug:` is a port
declaration, exactly like `ports:`, and it behaves like one: your image
already knows which port its debugger listens on, because you wrote the
`Dockerfile`. Telling it a second time, in a second place, would only create
somewhere for the two numbers to disagree.

What `debug:` buys over just adding `9229` to `ports:` is the *meaning*:
fghj knows which of this node's ports is the debugger, so the UI can label
that address and offer the halt switch for this node.

### The one variable fghj sets

| Variable | When | Value |
|---|---|---|
| `FGHJ_DEBUG_WAIT` | only when you flip the switch in the UI | `1` |

When it is off it is **absent**, never `0`, so `[ -n "$FGHJ_DEBUG_WAIT" ]`
and `os.environ.get("FGHJ_DEBUG_WAIT")` both do the right thing.

It means *halt before user code and wait for an IDE to attach*. It is off by
default for every container, you turn it on per container from the UI, and
the node is recreated to apply it. Use it when you need a breakpoint
somewhere that runs during startup — otherwise your process reaches it long
before any IDE can connect.

This is the only variable because it is the only fact an image cannot
determine on its own. Everything else — which port, which debugger, whether
to load it at all — you already decided when you built the image.

[Tutorial chapter 7](/tutorial/07-attaching-a-debugger/) runs both halves of
this end to end: a Python breakpoint catching a request in flight, and the
halt switch catching Go startup code that attaching cannot reach.

## Recipes

Each of these goes in **your** `Dockerfile` (or your entrypoint script). The
shape is the same every time:

- **Always listen.** A debugger that is listening but not halted runs at
  full speed; a breakpoint only exists once an IDE has attached and sent
  one. There is nothing to gate, so don't gate it.
- **Guard only the halt.** `FGHJ_DEBUG_WAIT` is the single branch.

This makes the image a *development* image, which is the honest description
of an image with a debugger compiled in. If you want one `Dockerfile` to
produce both, use a build stage (`build.target`) or pick your own
environment variable and set it with `environment:` — fghj has no opinion
about either.

### Node

```dockerfile
COPY docker-entrypoint.sh /usr/local/bin/
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["node", "server.js"]
```

```bash title="docker-entrypoint.sh"
#!/bin/sh
set -e
flag="--inspect=0.0.0.0:9229"
[ -n "$FGHJ_DEBUG_WAIT" ] && flag="--inspect-brk=0.0.0.0:9229"
export NODE_OPTIONS="$flag ${NODE_OPTIONS:-}"
exec "$@"
```

`0.0.0.0` is required: Node's inspector binds `127.0.0.1` by default, which
inside a container means nothing outside it can connect. Match the number in
`debug: 9229`, and attach to `api.shop.fghj.raw.internal:9229`.

### JVM (Java, Kotlin, Scala)

```bash title="docker-entrypoint.sh"
#!/bin/sh
set -e
suspend=n
[ -n "$FGHJ_DEBUG_WAIT" ] && suspend=y
export JAVA_TOOL_OPTIONS="-agentlib:jdwp=transport=dt_socket,server=y,suspend=$suspend,address=*:5005 ${JAVA_TOOL_OPTIONS:-}"
exec "$@"
```

The `*` in `address=*:<port>` is not optional on JDK 9 and later: a bare
`address=5005` binds loopback only. `suspend=y` is the JDWP spelling of
"wait for the debugger". Then `debug: 5005`, the conventional port.

### Python

Python has a better option than an entrypoint shim — a `sitecustomize.py`
anywhere on `PYTHONPATH` is imported by *every* Python process in the
container, so this survives `gunicorn`, forks, and shell wrappers without
having to be PID 1:

```dockerfile
RUN pip install debugpy
COPY sitecustomize.py /usr/local/lib/fghj/
ENV PYTHONPATH=/usr/local/lib/fghj:$PYTHONPATH
```

```python title="sitecustomize.py"
import os

# Firing for every process is the point, but only one of them can own the
# port. Setting our own marker in os.environ means children inherit it and
# skip — this is the kind of variable that *is* worth having, and it is
# yours, not fghj's.
if not os.environ.get("APP_DEBUGPY_LISTENING"):
    os.environ["APP_DEBUGPY_LISTENING"] = "1"
    import debugpy

    debugpy.listen(("0.0.0.0", 5678))
    if os.environ.get("FGHJ_DEBUG_WAIT"):
        debugpy.wait_for_client()
```

Then `debug: 5678`. One caveat: `python -S` and `python -E` both bypass this
entirely (`-S` skips `site`, `-E` ignores `PYTHONPATH`). See
[Python services in fghj](/guides/python/) for the rest — healthchecks
without `curl`, migrations as tasks, live reload.

### Ruby

```bash title="docker-entrypoint.sh"
#!/bin/sh
set -e
export RUBYOPT="-rdebug/open ${RUBYOPT:-}"
export RUBY_DEBUG_HOST=0.0.0.0
export RUBY_DEBUG_PORT=12345
[ -z "$FGHJ_DEBUG_WAIT" ] && export RUBY_DEBUG_NONSTOP=1
exec "$@"
```

Note the inversion: `debug/open` waits by default, so `RUBY_DEBUG_NONSTOP=1`
is set when you *don't* want to halt. `debug: 12345` matches `debug.gem`'s
own default.

### Go (Delve)

Go is the one runtime where the debugger has to be the launcher, so most of
this is a build-stage change rather than a flag:

```dockerfile
FROM golang:1.25 AS build
WORKDIR /src
COPY . .
# -N -l disables inlining and optimisation; without it stepping and variable
# inspection are close to useless. Do NOT pass -ldflags="-s -w" — it strips
# the DWARF info Delve needs.
RUN go build -gcflags="all=-N -l" -o /out/app ./cmd/app
RUN go install github.com/go-delve/delve/cmd/dlv@latest

FROM debian:bookworm-slim
COPY --from=build /out/app /usr/local/bin/app
COPY --from=build /go/bin/dlv /usr/local/bin/dlv
COPY docker-entrypoint.sh /usr/local/bin/
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["app"]
```

```bash title="docker-entrypoint.sh"
#!/bin/sh
set -e
bin="$(command -v "$1")"
shift
cont="--continue"
[ -n "$FGHJ_DEBUG_WAIT" ] && cont=""
exec dlv exec "$bin" \
  --headless --listen=0.0.0.0:2345 \
  --api-version=2 --accept-multiclient $cont -- "$@"
```

Then `debug: 2345`. Without `--continue` Delve halts before `main`, which is
exactly what `FGHJ_DEBUG_WAIT` is for. `--accept-multiclient` lets you
detach and reattach without killing the process.

If you'd rather not ship Delve in your production image, put the debug
tooling in its own build stage and point `build.target` at it — worked out
in full in [Go services in fghj](/guides/go/), along with the healthcheck
problem a shell-less image creates.

### PHP (Xdebug)

Xdebug is the exception to everything above, and it is worth knowing why.
Classic PHP is one process per request, so there is no long-lived process
for an IDE to attach *to* — the direction is reversed: your container dials
*out* to the IDE, which is the server. `debug:` is therefore the wrong tool
and you don't declare it. Use `extra_hosts` and `environment` instead:

```yaml
services:
  web:
    build:
      context: .
    extra_hosts:
      - "host.docker.internal:host-gateway"
    environment:
      XDEBUG_MODE: debug
      XDEBUG_CONFIG: "client_host=host.docker.internal client_port=9003"
```

Set `xdebug.start_with_request=trigger` in your ini and use a browser
extension to start a session per request — no restart, no always-on cost.

## Attaching

The address is the node's raw domain plus the port you declared:

```
api.shop.fghj.raw.internal:9229
```

That works from your host (your IDE) and from inside the run's network. It
does not collide with the same port on another node, or with the same node
in a parallel run, because each node has its own address.

In VS Code:

```json title=".vscode/launch.json"
{
  "type": "node",
  "request": "attach",
  "name": "api (fghj)",
  "address": "api.shop.fghj.raw.internal",
  "port": 9229,
  "localRoot": "${workspaceFolder}",
  "remoteRoot": "/app"
}
```

`localRoot`/`remoteRoot` matter: your breakpoints are on paths in your
checkout, and the debugger knows paths inside the container. Getting this
pair wrong is the usual cause of "breakpoint set but never hit" — the
debugger is attached and simply disagrees about which file you meant. The
equivalent in JetBrains IDEs is a path mapping on the remote debug
configuration.

## While halted, fghj stops health-checking

A process stopped before line 0 cannot report healthy, so turning on
`FGHJ_DEBUG_WAIT` also drops the container's `healthcheck`. Without that,
fghj would spend the node's entire health budget waiting on a container
that is waiting on you, and stall everything downstream.

The switch is off by default for every container, and it is **not**
something you declare in `.fghj.yaml` — that file is committed and shared,
so pinning it there would halt every teammate's start of that node
indefinitely.

## Security

A debug port is arbitrary code execution: anything attached can evaluate
expressions, call functions, and read every secret in the process.

In fghj it is local-only by construction. Published ports bind `127.0.0.1`
only, and raw-zone addresses are loopback aliases that RFC 1122 forbids from
leaving the machine. There is no setting that exposes a debug port to a
network. Still: `debug:` lives in a committed file, so treat declaring it as
a statement about a development environment and nothing else.
