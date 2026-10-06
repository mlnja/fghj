---
title: 7. Attaching a debugger
description: Two new repos, in Python and Go — catching a request in flight with debugpy, and catching startup code with Delve and the halt switch.
sidebar:
  order: 7
---

The shop works. This chapter is about the two kinds of debugging, which look
different enough that it's worth doing both:

- **Catching a request in flight.** Attach to a running process, set a
  breakpoint, make the request. The process is already up; you're
  interrupting it.
- **Catching startup.** The code you want runs before any IDE can connect.
  Attaching is too late by definition, so the container has to agree not to
  start until you say so.

We add a repo for each — `pricing` in Python and `search` in Go — because
the first case is where a dynamic language's hooks are nicest and the second
is where Go's compiled, launcher-driven debugger makes the problem obvious.

The whole of fghj's part in this is one line of config:

```yaml
debug: 5678
```

That publishes the port. It injects nothing, starts nothing, and never
touches your `Dockerfile`. Everything else below is your image doing its own
job — which is the point: fghj has no idea what a debugger is.

## Part 1 — a Python service, and a number nobody ordered

```bash
mkdir -p ~/code/shop/pricing
cd ~/code/shop/pricing
git init
git remote add origin https://github.com/you/pricing.git
```

`app.py` — the standard library and nothing else, like the rest of this
tutorial:

```python
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

NET_PRICES = {"Reading lamp": 42, "Oak stool": 79, "Wool blanket": 55}
VAT_RATE = 0.2


def gross(name):
    net = NET_PRICES[name]
    return round(net * (1 + VAT_RATE), 2)


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/health":
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"ok")
            return
        body = json.dumps({name: gross(name) for name in NET_PRICES}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        print("pricing: " + fmt % args, flush=True)


if __name__ == "__main__":
    port = int(os.environ.get("PORT", "8000"))
    print(f"pricing listening on {port}", flush=True)
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
```

`sitecustomize.py` — the debugger half:

```python
import os

import debugpy

debugpy.listen(("0.0.0.0", 5678))
if os.environ.get("FGHJ_DEBUG_WAIT"):
    print("pricing: waiting for a debugger on 5678", flush=True)
    debugpy.wait_for_client()
```

Three things about that file, and they're the whole contract:

- **It listens unconditionally.** A listening debugger runs at full speed
  and can't stop anything, because a breakpoint doesn't exist until an IDE
  has attached and sent one. There's nothing to gate, so nothing is gated.
- **The port is written literally.** fghj doesn't tell the image which port
  to use — the image already knows, because you wrote it. `debug: 5678`
  below is the same fact told to fghj so it can publish and address it,
  exactly like `ports:`.
- **`FGHJ_DEBUG_WAIT` is the one variable fghj sets**, only when you flip
  the switch in the UI, and only for the container you flipped it on.

`sitecustomize.py` is special to Python: *every* Python process imports it at
startup if it's on `PYTHONPATH`. That's why this works without being PID 1 —
it would survive gunicorn, a shell wrapper, or a fork. (With several
processes you'd want a marker variable so only one binds the port; see the
[Python guide](/guides/python/). One process here, so no marker.)

`Dockerfile`:

```docker
FROM python:3.13-slim
WORKDIR /app
RUN pip install --no-cache-dir debugpy
COPY sitecustomize.py /usr/local/lib/fghj/
COPY app.py ./
ENV PYTHONPATH=/usr/local/lib/fghj PYTHONUNBUFFERED=1
CMD ["python", "app.py"]
```

`PYTHONUNBUFFERED=1` is not optional if you want to see those `print`s in
the UI's log pane — without it they sit in a pipe buffer until the process
exits.

And `.fghj.yaml`:

```yaml
version: "1.0"

services:
  prices:
    build:
      context: .
    ports:
      "8000":
        primary: true
    debug: 5678
    environment:
      PORT: "8000"
    healthcheck:
      test: ["CMD", "python", "-c", "import urllib.request; urllib.request.urlopen('http://localhost:8000/health')"]
      interval: 2
      retries: 15
```

The service is called `prices`, not `api` — `catalog` already has an `api`,
and a leaf name matching two siblings has to be qualified
(`${FGHJ_SERVICE_FQDN:pricing::api}`) instead of just written. Naming them
apart is easier. The id becomes `prices.pricing`.

Note the healthcheck: `python:3.13-slim` ships no `curl`, so it polls itself
with the interpreter it already has. `urlopen` raises on a non-2xx, which
exits non-zero, which is what Docker reads as unhealthy.

```bash
fghj validate .fghj.yaml
```

### Wire it into the storefront

In `storefront/.fghj.yaml`, add the dependency and the address:

```yaml
      PRICING_URL: http://${FGHJ_SERVICE_FQDN:prices}:8000
```

```yaml
      - kind: service
        repo: https://github.com/you/pricing.git
```

And in `storefront/server.js`, fetch it next to the catalog items:

```js
async function grossPrices() {
  const res = await fetch(`${process.env.PRICING_URL}/`);
  if (!res.ok) throw new Error(`pricing answered ${res.status}`);
  return res.json();
}
```

with the list line becoming:

```js
const gross = await grossPrices().catch(() => ({}));
items = (await catalogItems())
  .map((item) => {
    const paid = gross[item.name];
    return `<li>${item.name} — $${item.price}${paid ? ` · you pay $${paid}` : ''}</li>`;
  })
  .join('');
```

**Start default environment**, and reload:

```
 • Reading lamp — $42 · you pay $50.4
 • Oak stool — $79 · you pay $94.8
 • Wool blanket — $55 · you pay $66
```

(Those are `round(..., 2)` values rendered by `JSON.stringify`, which drops
the trailing zero — a second bug, and not the one we're here for.)

Where does $50.4 come from? You can read it off `app.py` in ten seconds —
which is the honest reason this tutorial's bug is a small one. Pretend you
can't, and let's go and look.

### The address

Open the `prices.pricing` node's drawer in the UI. Under the ports you now
have an extra row:

```
5678   debug   prices.pricing.shop.fghj.raw.internal:5678  ⧉
```

That address is not a special debugging feature — it's the ordinary raw
zone, the same mechanism `${FGHJ_SERVICE_FQDN:db}` used in chapter 2.
`debug: 5678` put the port in the published set and fghj addressed it like
any other. Which means it works from your host *and* from inside the run's
network, on the number you declared, and it doesn't collide with the same
port on another node or with the same node in a parallel run.

The row says *declared*, not *listening*. fghj published a port; whether
anything answers on it is your image's business. If you'd forgotten the
`pip install debugpy`, the port would be there and the row would look
exactly the same.

### Attach, and catch a request

`.vscode/launch.json`, in the `pricing` repo:

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "name": "pricing (fghj)",
      "type": "debugpy",
      "request": "attach",
      "connect": {
        "host": "prices.pricing.shop.fghj.raw.internal",
        "port": 5678
      },
      "pathMappings": [{ "localRoot": "${workspaceFolder}", "remoteRoot": "/app" }],
      "justMyCode": false
    }
  ]
}
```

`pathMappings` is the part people get wrong: your breakpoints are on paths in
your checkout, the debugger knows paths inside the container (`/app/app.py`).
Mismatch there is the usual cause of "breakpoint set but never hit" — the
debugger is attached and simply disagrees about which file you meant.

Run the config, put a breakpoint on `net = NET_PRICES[name]`, and reload
`https://web.storefront.shop.fghj.internal`.

The page hangs. The breakpoint hits. In the debug console:

```
> net
42
> VAT_RATE
0.2
> gross("Oak stool")
94.8
```

There's your $50.4. Press continue three times (once per item) and the page
finishes rendering, late but intact.

Two honest notes about what just happened:

- **The whole container paused, not just that request.** debugpy suspends
  every thread by default, so the healthcheck stopped answering too. Sit on
  a breakpoint long enough and Docker will mark the node unhealthy. Nothing
  reacts to that — fghj only waits on health while *starting* a node, never
  after — but the pill in the UI is telling the truth.
- **`web` was already up**, so it just waited on a slow HTTP call and
  eventually rendered. If you'd been halted *before* the first request, you'd
  have seen `catalog unavailable`-style degradation instead, which is the
  next part.

## Part 2 — a Go service, and code that runs before you can attach

```bash
mkdir -p ~/code/shop/search
cd ~/code/shop/search
git init
git remote add origin https://github.com/you/search.git
```

`go.mod`:

```
module shop/search

go 1.25
```

`catalog.txt`:

```
Reading lamp
Oak stool
Wool blanket
```

`main.go`:

```go
package main

import (
	"encoding/json"
	"log"
	"net/http"
	"os"
	"strings"
)

// Package initialisation: this runs before main, and long before anything
// could attach to the running process. Remember this line.
var index = buildIndex()

func buildIndex() []string {
	raw, err := os.ReadFile("/app/catalog.txt")
	if err != nil {
		log.Printf("no catalog: %v", err)
		return nil
	}
	var items []string
	for _, line := range strings.Split(string(raw), "\n") {
		if line = strings.TrimSpace(line); line != "" {
			items = append(items, line)
		}
	}
	return items
}

func main() {
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}

	// A healthcheck for an image with no curl and no shell: the binary
	// checks itself. Docker runs a healthcheck command directly, not
	// through the entrypoint, so this never goes near Delve.
	if len(os.Args) > 1 && os.Args[1] == "-healthcheck" {
		resp, err := http.Get("http://localhost:" + port + "/health")
		if err != nil || resp.StatusCode != http.StatusOK {
			os.Exit(1)
		}
		return
	}

	http.HandleFunc("/health", func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte("ok"))
	})
	http.HandleFunc("/search", func(w http.ResponseWriter, r *http.Request) {
		q := strings.ToLower(r.URL.Query().Get("q"))
		hits := []string{}
		for _, item := range index {
			if strings.Contains(strings.ToLower(item), q) {
				hits = append(hits, item)
			}
		}
		w.Header().Set("content-type", "application/json")
		json.NewEncoder(w).Encode(hits)
	})

	log.Printf("search listening on %s, %d items indexed", port, len(index))
	log.Fatal(http.ListenAndServe(":"+port, nil))
}
```

`Dockerfile` — Go is the one runtime where the debugger is the *launcher*,
so Delve ships in the image and starts the program:

```docker
FROM golang:1.25 AS build
WORKDIR /src
COPY go.mod main.go ./
# -N -l disables inlining and optimisation. Without it, stepping jumps
# around and half the variables read as "optimized out". Never pair this
# with -ldflags="-s -w" — that strips the DWARF info Delve needs.
RUN go build -gcflags="all=-N -l" -o /out/search .
RUN go install github.com/go-delve/delve/cmd/dlv@latest

FROM debian:bookworm-slim
COPY --from=build /out/search /usr/local/bin/search
COPY --from=build /go/bin/dlv /usr/local/bin/dlv
COPY catalog.txt /app/catalog.txt
COPY docker-entrypoint.sh /usr/local/bin/
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["search"]
```

`docker-entrypoint.sh`:

```bash
#!/bin/sh
set -e
# Peel the binary off the front of CMD so what's left is its arguments.
bin="$(command -v "$1")"
shift
cont="--continue"
[ -n "$FGHJ_DEBUG_WAIT" ] && cont=""
exec dlv exec "$bin" \
  --headless --listen=0.0.0.0:2345 \
  --api-version=2 --accept-multiclient $cont -- "$@"
```

```bash
chmod +x docker-entrypoint.sh
```

Don't skip that. Docker copies the mode from disk, and a non-executable
entrypoint fails the container with a bare `permission denied` that doesn't
mention the script.

`.fghj.yaml`:

```yaml
version: "1.0"

services:
  index:
    build:
      context: .
    ports:
      "8080":
        primary: true
    debug: 2345
    environment:
      PORT: "8080"
    healthcheck:
      test: ["CMD", "search", "-healthcheck"]
      interval: 2
      retries: 15
```

Then in `storefront/.fghj.yaml`:

```yaml
      SEARCH_URL: http://${FGHJ_SERVICE_FQDN:index}:8080
```

```yaml
      - kind: service
        repo: https://github.com/you/search.git
```

and in `storefront/server.js`, below the items:

```js
let found = '';
try {
  const hits = await (await fetch(`${process.env.SEARCH_URL}/search?q=lamp`)).json();
  found = `<p>search for "lamp": ${hits.join(', ') || 'nothing'}</p>`;
} catch (err) {
  found = `<p>search unavailable: ${err.message}</p>`;
}
```

**Start default environment.** Both new nodes build and come up, and the page
gains:

```
search for "lamp": Reading lamp
```

`--continue` is why that worked: Delve launched the program, listened on
2345, and let it run at full speed. The debugger being there costs you
nothing until you attach.

### The thing you cannot attach to

Put a breakpoint in `buildIndex` and attach. You'll never hit it. The
function ran during package initialisation, before `main`, before the server
bound a port — long before your IDE finished its handshake. No amount of
being quick about it helps; this is a race you lose by construction.

This is the one case `debug:` alone can't reach, and the only reason there's
a switch at all.

### Halt at startup

In the `index.search` drawer, under the debug row:

```
halt at startup    [ off ]
```

Click it. The node is recreated, and three things are now true:

1. **`FGHJ_DEBUG_WAIT=1` is in the container's environment** — the single
   variable fghj sets for debugging. Your entrypoint reads it and drops
   `--continue`, so Delve halts before anything runs.
2. **fghj stopped health-checking this node.** A process stopped before line
   0 can never report healthy; leaving the check in place would make fghj
   spend the node's whole two-minute allowance waiting for a container that
   is waiting for you.
3. **Nothing downstream is held back.** That's the flip side of (2): a node
   is waited on *because* it declares a healthcheck, so dropping the check
   also drops the wait. `web` starts immediately and renders `search
   unavailable: fetch failed`, which is the honest report — the search
   service really isn't answering.

The node card still says `running`, and that's correct rather than a lie:
the container exists and its process is alive. It just hasn't started.

Now attach — Delve's own client will do:

```bash
dlv connect index.search.shop.fghj.raw.internal:2345
```

```
(dlv) break main.buildIndex
Breakpoint 1 set at 0x... for main.buildIndex() ./main.go:17
(dlv) continue
> main.buildIndex() ./main.go:17 (hits goroutine(1):1 total:1)
(dlv) next
(dlv) print string(raw)
"Reading lamp\nOak stool\nWool blanket\n"
(dlv) continue
search listening on 8080, 3 items indexed
```

The same from VS Code, in the `search` repo:

```json
{
  "name": "index (fghj)",
  "type": "go",
  "request": "attach",
  "mode": "remote",
  "host": "index.search.shop.fghj.raw.internal",
  "port": 2345,
  "substitutePath": [{ "from": "${workspaceFolder}", "to": "/src" }]
}
```

`substitutePath` is Go's version of the mapping problem: the DWARF info
records the paths the *build stage* saw (`/src/main.go`), not the ones in
your checkout.

Reload the storefront and search works again. Then click **halt at startup**
off: the container is recreated once more, this time without the variable,
and the node comes back the ordinary way.

### Why that switch isn't in `.fghj.yaml`

Everything else in this tutorial lives in a committed file. This doesn't,
and the reason is the same reason the file is committed: "halt this node
before line 0 and wait for a human" pinned in `.fghj.yaml` would block
*every* teammate's start of that node, indefinitely, and stall everything
downstream of it. It isn't a property of the node. It's something one person
is doing to one container for the next ten minutes.

So it lives on the container, is flipped per container, and is written down
only so that an `fghjd` restart doesn't silently drop you out of a session.

### And why it isn't drift

Press **Start default environment** while `index.search` is halted. Nothing
happens to it.

That's deliberate, and it's the load-bearing decision of the whole feature.
A top-up compares each node's config against what's running and recreates
anything that drifted — see
[chapter 6](/tutorial/06-when-it-goes-wrong/). If the halt switch counted as
config, every top-up would read your halted container as wrong and recreate
it, destroying the session you just set up. So it's applied *after* the
config hash is taken, not folded into it.

The cost, stated plainly: a recreate for *genuine* drift — you edited
`.fghj.yaml` — does drop the flag. The switch then flips itself off in the
UI, because the new container really doesn't have it. That's the honest
failure mode of the two available, and it's the one fghj picked.

## What fghj did, and didn't

It published two ports and set one environment variable.

It did not modify either `Dockerfile`, derive a second image from one, mount
a shim, inspect an entrypoint, or learn what debugpy or Delve are. Both
recipes above are things you'd have written anyway to debug these containers
by hand; `debug:` is how you tell fghj that one of your ports is the
interesting one, so it can print the address and offer the switch.

That's also why there's no list of supported languages. There's nothing to
support.

---

That's the tutorial. From here:

- [Attaching a debugger](/guides/debugging/) — the contract in one page,
  plus recipes for Node, the JVM, Ruby, and PHP/Xdebug (which is the
  exception: it dials out to your IDE, so `debug:` is the wrong tool).
- [Python services in fghj](/guides/python/) and
  [Go services in fghj](/guides/go/) — the rest of what these two runtimes
  need: healthchecks without curl, migrations as tasks, live reload, private
  modules, graceful shutdown.
- [.fghj.yaml reference](/reference/fghj-yaml/) — the page to keep open.
