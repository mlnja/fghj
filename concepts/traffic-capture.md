# Traffic capture

The node drawer's **Traffic** tab is a DevTools Network panel for one
service. It shows every HTTP request fghj's proxies forwarded to that node,
from the host and from other containers, with headers, bodies and timing. This guide covers where the capture
happens, what is kept, and why the controls are as few as they are.

## Where it is captured: the proxy that is already in the path

Everything that reaches a service by name over `https://…fghj.internal`
already goes through fghjd's reverse proxy ([[local-ca-and-tls-proxy]]). The
proxy speaks HTTP/1.1 on both legs, so it already sees each request as a
request: method, URL, headers and a body stream. Capturing there costs no
new hop, no agent inside the container and no extra port.

The raw zone (`fghj.raw.internal`, [[two-zones-and-raw-ports]]) is not
proxied, so its traffic is never seen. That's the trade the raw zone already
makes.

Requests from a container to another container's `fghj.internal` name go
through the in-network sidecar instead ([[in-network-sidecar]]). It runs the
same `web::proxy` and `TrafficStore` code in its own container, and fghjd
pulls what it records into the same per-node log the host proxy fills, so one
list shows both. See *How the sidecar's traffic reaches fghjd* below.

## Recording is fghjd's state, not the drawer's

Recording is switched on per node and held in fghjd
(`TrafficStore` on `WorkspaceRegistry`, keyed by workspace id and node id).
Closing the drawer, opening another node or reloading the page doesn't stop
it. Only the Stop button or a fghjd restart stops it. Clearing the list
doesn't stop it either. The point is to start recording, go reproduce the bug
in the browser, and come back to find it recorded. A recording tied to an
open tab would have stopped by then. The graph shows a pulsing red dot on
every node that is recording, polled from `GET /traffic/recording` alongside
`/environment`, so a forgotten recording stays visible.

The mode is read on **every request**, not once per connection. The HTTPS
side resolves its backend once per connection, by SNI, and a browser keeps
that connection alive for minutes. So `Backend` carries the node's
`Arc<TrafficLog>` rather than a recording flag, and switching recording on
catches the very next request on a connection that is already open.

## In memory only

Captured traffic never touches SQLite or the disk. It is debugging output
for the current session. It holds session cookies, bearer tokens and request
bodies, and writing those to a file that outlives the session would be a
liability with no upside.

Bounds, all fixed:

- per node, a ring of 1000 exchanges or 64 MiB, whichever is hit first.
  The oldest exchange is evicted, but never the newest, so one huge exchange
  is still visible.
- 128 MiB across all nodes, enforced by the node whose insert crosses it,
  so a second forgotten recording can't double the cost.
- 8 MiB per stored body. The wire size is still counted past it, and the UI
  says "kept the first 8 MiB of N".

Bodies are stored as they went over the wire (gzip, br and deflate
included) and decoded only when someone opens them, with a 32 MiB limit on
the decoded size. Decoding every body on the request path would cost CPU
for bodies nobody looks at.

## What is kept: decided by content type

There is one control, the body mode in the recording menu:

| Mode | Keeps |
|---|---|
| **Text** (default) | Bodies you'd read: `text/*`, JSON (and `*+json`, `x-ndjson`), XML (and `*+xml`), form-urlencoded, JavaScript, GraphQL. Binary types (`image/*`, video, audio, fonts, `octet-stream`, zip, pdf, wasm, protobuf) are recorded by type and size only. A body with no or an unknown content type is kept, then dropped at the end unless it's UTF-8 with no NULs. Multipart keeps its text fields and records each file as name, type and size. |
| **All** | Every body up to the cap. Images get a preview. |
| **Headers only** | No bodies, just sizes. |

There is deliberately no editable content-type list and no adjustable cap.
The line between
"read it" and "it's a blob" is the same for every app, and a knob for it
would be one more thing to explain and get wrong. A JSON response of 100 lines
is always kept in full, because Text mode keeps text whole up to 8 MiB.

`text/event-stream` and other long-lived bodies are captured as they flow,
up to the cap. Their row stays "streaming" until the stream ends.

## How a body is copied without slowing it

`TapBody<B>` wraps a hyper body and passes every frame through unchanged,
copying data frames into the exchange as they go. It finishes the exchange
on end-of-stream, on error, or on drop. A drop before the end counts as
`aborted`, for example a browser that navigated away mid-download. A detail
hyper forces: hyper stops polling a body once `is_end_stream()` says it's
done, so a fully read body can be dropped without ever yielding `None`. The
drop handler checks `is_end_stream()` to tell a finished body from an
aborted one. `size_hint` is delegated, so wrapping a body never changes its
framing on the wire.

When recording is off, `TrafficLog::begin` returns `None` and the request
takes exactly the path it took before: no wrapper, no copy.

## What a record holds

The client's request headers are recorded **before** the proxy adds
`X-Forwarded-*` and strips hop-by-hop headers, so the record matches what the
browser's own DevTools would show. The response headers are recorded after
hop-by-hop stripping, which is also what the browser received. A backend that
couldn't be reached is recorded as the 502 the client got, together with the
error. An upgrade (`101`, shown as `101 ⇄`) is recorded as open until its
relay ends, then closed with its duration.

`source` is `host` for anything arriving on the host proxy over loopback,
which covers browsers, curl, and containers reaching the host
through Docker's port mapping. A request through the sidecar names the node
that sent it: fghjd maps the caller's IP to a node by inspecting the
network. An IP that isn't a node, or one seen before the lookup returned, is
shown as the IP.

## How the sidecar's traffic reaches fghjd

The sidecar can't reach fghjd. A host Unix socket bind-mounted into a
container gives `Connection refused` on OrbStack, and handing it fghjd's
socket would hand it the whole control API anyway. So it goes the other way:
the sidecar listens on port 7480, Docker publishes that on the host's
127.0.0.1 with a port it picks, and fghjd dials in (`web::capture`,
`daemon::capture`).

One long `POST /capture` carries both directions. fghjd streams the
recording set up as JSON lines (node id to mode), a new line whenever it
changes. The sidecar streams back every exchange each time it changes, as one
JSON line holding all of it, bodies base64'd. fghjd folds those into its log
keyed by the sidecar's run and the sidecar's id, so the steps of one exchange
update one row.

**The sidecar records only while that request is open.** The recording set
comes over the connection, not from a file. When fghjd stops recording the
last node of a workspace, restarts or crashes, the connection ends, and the
sidecar stops and clears. Nothing is recorded that nobody will see, and a
fghjd restart can't leave a sidecar recording from a stale setting.

fghjd keeps a connection only while some node of the workspace is
recording, and reconnects every second if it drops. Exchanges that started
before a Clear are not let back in by a later update.

The port is reachable from the host's loopback, including web pages, and
from every container on the network, so the request must carry a token.
fghjd writes a random one into the sidecar's read-only routes folder
(`capture-token`), and the sidecar reads it on each request. Containers don't
have that folder; pages and other processes don't know the token.

A sidecar started by an older fghj has no capture port. It isn't replaced to
get one, because its IP is in every running node's `--dns`. It gets one when
the environment is recreated. Until then fghjd logs why it can't capture.

## API

All endpoints take `?workspace=<id>`:

- `GET /traffic/recording`: node id to mode, for every recording node.
- `POST /nodes/{node}/traffic/recording` with `{"mode": "text"|"all"|"none"|null}`.
  `null` stops recording.
- `POST /nodes/{node}/traffic/clear`.
- `GET /nodes/{node}/traffic/stream`: SSE. It sends a snapshot first, then
  `upsert`/`mode`/`cleared` events. A subscriber that lags behind the
  broadcast channel gets a fresh snapshot instead of a gap. It subscribes
  before taking the snapshot, so nothing falls between the two. A duplicate
  upsert is harmless.
- `GET /nodes/{node}/traffic/{id}`: one exchange with decoded bodies.
- `GET /nodes/{node}/traffic/{id}/body/{request|response}`: the decoded
  bytes with their original content type, for image previews and downloads.
  This is arbitrary app content served from fghj's own origin, so it is sent
  with `Content-Security-Policy: sandbox`, `nosniff`, and, unless it's an
  image, `Content-Disposition: attachment`. A recorded HTML page must not run
  script against the control API.

This is the one place the UI uses SSE besides logs ([[ui-architecture]]).
Requests arrive in bursts, and a 1s poll would either miss the order they
arrived in or ship the whole list every second.

## Status

Implemented: `src/web/traffic.rs` (store, ring, classification, `TapBody`,
multipart summary, decoding, shipping and ingest), the hooks in
`src/web/proxy.rs` (`forward`, `splice_upgraded`), `query::resolve_route`
returning a `RouteTarget`, `src/web/api/traffic.rs`, the sidecar link in
`src/web/capture.rs` and `src/daemon/capture.rs`, and
`ui/src/lib/TrafficTab.svelte` plus the dot in `GraphView.svelte`.

Deliberately not planned: an outgoing view. A node's call to another node is
already in the callee's list, with the caller as its source. Calls to the
internet aren't fghj's to show.

Not yet: HTTP/2, and request timing finer than time-to-first-byte and total.
