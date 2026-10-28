---
title: 7. Attaching a debugger
description: Node's inspector in the api and the worker — catching a request in flight, and catching work that starts before you can attach, with the halt switch.
sidebar:
  order: 7
---

The app works. This chapter is about the two kinds of debugging, which are
different enough that it's worth doing both:

- **Catching a request in flight.** Attach to a running process, set a
  breakpoint, make the request. The process is already up, and you're
  interrupting it.
- **Catching startup.** The code you want runs before any IDE can connect.
  Attaching is too late by definition, so the container has to agree not to
  start until you say so.

The api is the first case. The worker is the second: on startup it drains
whatever is waiting in its queue, and it's done before you've clicked
anything.

```bash
git -C files-api checkout ch7
git -C files-worker checkout ch7
git -C files-api diff ch6 ch7
```

The whole of fghj's part in this is one line of config in each repo:

```yaml
    debug: 9229
```

That publishes the port. It injects nothing, starts nothing, and never
touches your `Dockerfile`. Everything else below is the image doing its own
job, which is the point: fghj has no idea what a debugger is.

## The image's half

Both repos gain the same `docker-entrypoint.sh`:

```sh
#!/bin/sh
# Node's inspector is always listening, so an IDE can attach at any time.
# fghj sets FGHJ_DEBUG_WAIT only when you flip "halt at startup" for this
# container, and then it waits for the IDE before running a line.
set -e
flag="--inspect=0.0.0.0:9229"
[ -n "$FGHJ_DEBUG_WAIT" ] && flag="--inspect-brk=0.0.0.0:9229"
export NODE_OPTIONS="$flag ${NODE_OPTIONS:-}"
exec "$@"
```

and two lines in the `Dockerfile` to run it ahead of the `CMD`:

```docker
COPY docker-entrypoint.sh /usr/local/bin/
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["node", "src/server.ts"]
```

Four things about that script, and they're the whole contract:

- **It listens unconditionally.** A listening debugger runs at full speed
  and can't stop anything, because a breakpoint doesn't exist until an IDE
  has attached and sent one. There's nothing to gate, so nothing is gated.
- **The port is written literally.** fghj doesn't tell the image which port
  to use. The image already knows, because you wrote it. `debug: 9229` is
  the same fact told to fghj so it can publish and address it, exactly like
  `ports:`.
- **`0.0.0.0` is required.** Node's inspector binds `127.0.0.1` by default,
  which inside a container means nothing outside it can connect.
- **`FGHJ_DEBUG_WAIT` is the one variable fghj sets**, only when you flip
  the switch in the UI, and only for the container you flipped it on. When
  it's off it's absent, never `0`, so `[ -n … ]` is the right test.

`NODE_OPTIONS` rather than a flag on the command line, because the `CMD`
stays the image's own: the entrypoint doesn't need to know what it's
starting. The migration and the seed are built from the same image and get
the inspector too, each in its own container. They don't declare `debug:`,
so nothing publishes it, and it costs nothing.

Remember from chapter 1 that Node runs the `.ts` files directly. The code in
the container is your source with the types blanked out, so line numbers
match and no source maps are involved.

## The address

**Start** the api to pick up the new image, and open its drawer. Under the
ports there's an extra row, the debug port:

```
9229   debug   api.files-api.files.fghj.raw.internal:9229  ⧉
```

That's the ordinary raw zone, the same mechanism `${FGHJ_SERVICE_FQDN:db}`
used in chapter 2: `debug: 9229` put the port in the published set, and fghj
addressed it like any other. Docker also published it on a localhost port,
as it does every port.

The row says *declared*, not *listening*. fghj published a port, and whether
anything answers on it is the image's business. If the entrypoint were
missing, the row would look exactly the same.

One thing about Node specifically: its inspector refuses discovery requests
whose `Host` header isn't an IP address or `localhost`, as a guard against
DNS rebinding. VS Code and Zed both attach through
[vscode-js-debug](https://github.com/microsoft/vscode-js-debug), which
sends `Host: localhost` whatever address it dials, so the raw name works
for them. A client that sends the real hostname, such as `chrome://inspect`,
is turned away. For those, use the localhost port Docker published:

```bash
docker port fghj-files-default-api-files-api 9229
```

It's picked fresh whenever the container is created, so ask again after a
Start, a Reset or a flip of the halt switch.

## Catch a request in flight

`files-api` ships an attach config for Zed, `.zed/debug.json`:

```json
[
  {
    "adapter": "JavaScript",
    "label": "api (fghj)",
    "type": "node",
    "request": "attach",
    "address": "api.files-api.files.fghj.raw.internal",
    "port": 9229,
    "localRoot": "$ZED_WORKTREE_ROOT",
    "remoteRoot": "/app"
  }
]
```

The VS Code equivalent, in `.vscode/launch.json`, is the same fields:

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "name": "api (fghj)",
      "type": "node",
      "request": "attach",
      "address": "api.files-api.files.fghj.raw.internal",
      "port": 9229,
      "localRoot": "${workspaceFolder}",
      "remoteRoot": "/app"
    }
  ]
}
```

The address has the workspace folder's name in it (`files`), so a
workspace with another name needs that segment changed.

`localRoot`/`remoteRoot` is the part people get wrong. Your breakpoints are
on paths in your checkout, and the debugger knows paths inside the
container (`/app/src/server.ts`, from the `WORKDIR`). A mismatch there is
the usual cause of "breakpoint set but never hit": the debugger is attached
and simply disagrees about which file you meant.

Run the config, and put a breakpoint in `present()` in `src/server.ts`, on
the line that signs the download link:

```ts
    downloadUrl: await presignGet(row.object_key, row.name),
```

Reload `https://files.test`. The file list doesn't appear, and the
breakpoint hits. In the debug console:

```
> row.name
'harbor.svg'
> row.object_key
'samples/harbor.svg'
> await presignGet(row.object_key)
'https://s3.files-api.files.fghj.internal/files/samples/harbor.svg?X-Amz-Algorithm=…'
```

That's chapter 2's decision showing up in a running process: the signed link
names the http-zone host, the one the browser can reach. Remove the
breakpoint and continue, and the list renders.

Two honest notes about what just happened:

- **The whole process paused, not just that request.** Node runs your code
  on one thread, so while it's stopped nothing else is served, including
  the healthcheck. Sit on a breakpoint for long enough and Docker marks the
  node unhealthy. Nothing reacts to that, because fghj waits on health only
  while *starting* a node, but the pill in the UI is telling the truth.
- **nginx is waiting too.** It gives an upstream 60 seconds by default
  before answering the browser with a 504. A long session at a breakpoint
  ends with an error page, which is honest about what happened.

## The thing you can't attach to

The worker is different. When it starts it pops jobs off Redis and
processes them straight away, and the ones already waiting are handled in
the first second. Put a breakpoint in `thumbnail()` and attach afterwards,
and those jobs are long gone. No amount of being quick about it helps:
this is a race you lose by construction.

To set it up, give the worker a backlog. Open `worker` and **Stop** it, then
open `seed` and **Start** it. The seed queues a job per sample, and with no
worker running they wait in Redis.

### Halt at startup

In the worker's drawer, under the debug row:

```
halt at startup    [ off ]
```

Click it. The node is recreated, and three things are now true:

1. **`FGHJ_DEBUG_WAIT=1` is in the container's environment.** It's the single
   variable fghj sets for debugging. The entrypoint reads it and passes
   `--inspect-brk`, so Node stops before the first line of `worker.ts`.
2. **fghj stopped health-checking this node.** A process stopped before line
   1 can never report healthy, and leaving the check in place would make fghj
   spend the node's whole allowance waiting for a container that's waiting
   for you. The worker has no healthcheck, but the api does, and the same
   switch on the api drops it.
3. **Nothing downstream is held back.** That's the flip side of (2): a node
   is waited on *because* it declares a healthcheck, so dropping the check
   also drops the wait.

The node card still says `running`, and that's correct: the container exists
and its process is alive. It just hasn't started.

Attach with `files-worker`'s own `.zed/debug.json`, which is the same
config pointed at `worker.files-worker.files.fghj.raw.internal`. The name
doesn't change when the halt switch recreates the container, so the config
doesn't either. The debugger stops on the first line. Put a breakpoint on
the first line of `thumbnail()`:

```ts
async function thumbnail(job: ThumbnailJob) {
  if (!job.contentType.startsWith("image/")) {
```

and continue. It hits at once, on the first job from the backlog:

```
> job
{ fileId: '…', key: 'samples/harbor.svg', contentType: 'image/svg+xml' }
```

Step through it if you like: fetch from S3, resize, upload, then the `PUT`
to the api that marks it done. Continue, and the rest of the backlog goes
through.

Then click **halt at startup** off. The container is recreated once more,
this time without the variable, and the node comes back the ordinary way.

### Why that switch isn't in `.fghj.yaml`

Everything else in this tutorial lives in a committed file. This doesn't,
and the reason is the same reason the file is committed: "halt this node
before line 1 and wait for a human", pinned in `.fghj.yaml`, would block
*every* teammate's start of that node, indefinitely, and stall everything
waiting on it. It isn't a property of the node. It's something one person is
doing to one container for the next ten minutes.

So it lives on the container, is flipped per container, and is written down
only so that an `fghjd` restart doesn't silently drop you out of a session.

### And why it isn't drift

Press **Switch to flow** while the worker is halted. Nothing happens to it.

That's deliberate, and it's the decision the whole feature rests on. A
switch compares each node's config against what's running and recreates
anything that drifted (see [chapter 6](/tutorial/06-when-it-goes-wrong/)).
If the halt switch counted as config, every switch would read your halted
container as wrong and recreate it, destroying the session you just set up.
So it's applied *after* the config hash is taken, not folded into it.

The cost, stated plainly: a recreate for *genuine* drift, because you
edited `.fghj.yaml`, does drop the flag. The switch then flips itself off in
the UI, because the new container really doesn't have it. Of the two
available failure modes, that's the honest one, and it's the one fghj
picked.

## What fghj did, and didn't

It published two ports and set one environment variable.

It didn't modify either `Dockerfile`, derive a second image from one, mount
a shim, inspect an entrypoint, or learn what Node's inspector is. The script
above is what you'd have written anyway to debug these containers by hand.
`debug:` is how you tell fghj that one of your ports is the interesting one,
so it can print the address and offer the switch.

That's also why there's no list of supported languages. There's nothing to
support.

---

That's the tutorial. From here:

- [Attaching a debugger](/guides/debugging/): the contract in one page,
  plus recipes for Python, the JVM, Ruby, Go and PHP/Xdebug (which is the
  exception: it dials out to your IDE, so `debug:` is the wrong tool).
- [.fghj.yaml reference](/reference/fghj-yaml/): the page to keep open.
- [Concepts](/concepts/architecture/): one page per subsystem, each
  explaining why it is the way it is.
