---
title: "Recording HTTP traffic between Docker containers"
description: How to see the requests your services send each other in local Docker development, why mitmproxy, tcpdump and logging middleware each fall short there, and how fghj records service-to-service calls with bodies and the calling service, with nothing added to your containers.
---

Browser DevTools show what the browser sent. In a system of several
services that's the smaller half: the browser calls the frontend, the
frontend's nginx calls the API, the API calls an auth service, S3 and a
webhook receiver. When something in that chain returns the wrong thing,
the question is *what exactly did service A send to service B, and what
came back?*, and DevTools can't see any of it.

## The usual ways to look

### A logging middleware in every service

Add request/response logging to each service. It's the most common answer
and the most expensive one: code in every repo and every language, bodies
that are usually left out because they're big or sensitive, and logs from
five containers to interleave by timestamp. A service you don't own
(MinIO, Keycloak, a third-party image) logs whatever it logs.

### mitmproxy as an HTTP proxy

Run mitmproxy in the Compose network and set `HTTP_PROXY`/`HTTPS_PROXY` in
every client container. It shows full requests and responses, and it's a
great tool. In a multi-service setup:

- Every client must honour the proxy variables, and many don't (gRPC
  clients, some SDKs, anything that opens its own sockets). `NO_PROXY`
  needs maintaining so the database driver doesn't try to go through it.
- HTTPS needs mitmproxy's CA trusted in every container, with the right
  variable per runtime.
- The flow list shows a destination, not a caller. Telling which container
  made a request means matching IPs by hand.
- It's one more service, and one more set of environment variables, to keep
  out of the config you commit.

### Reverse-proxy mode, per service

mitmproxy (or an Envoy sidecar) in front of a single service, with the
callers pointed at the proxy's name instead of the real one. It works, for
the one service you rewired, and you rewire again for the next one.

### tcpdump and Wireshark

Capture on the Docker bridge, or from a `nicolaka/netshoot` container
sharing the target's network namespace. You see every packet, including
non-HTTP ones. You don't see HTTPS contents without the session keys, and
on macOS the bridge lives inside Docker Desktop's VM, not on your machine.

### Tracing

OpenTelemetry gives you timings and the call tree across services, which
is a different and complementary question. Spans don't carry request and
response bodies, and they only exist for the services you instrumented.

All of these work. What they share is that **the thing that sees the
traffic isn't already in the path**, so you put it there, per service, per
runtime, and take it out again afterwards.

## Recording with fghj

In fghj, every call to a service's `*.fghj.internal` name already goes
through one of fghj's own proxies: the one on your machine for the browser
and your terminal, and the workspace's [sidecar](/concepts/sidecar/) for
containers. Both can record what they forward.

Click a service in fghj's UI, open its **Traffic** tab, and press
**⏺ Record**. Then use the app:

```
POST  200  /api/files                web.files-web
POST  200  /api/files/…/complete     web.files-web
PUT   200  /files/uploads/…          host
HEAD  200  /files/uploads/…          api.files-api
```

- **The caller is named.** The last column is the source: a container's
  node, looked up by the sidecar from the connection, or `host` for your
  browser and anything else on your machine. Click a source to filter by
  it.
- **Bodies included.** Each row opens to both sides' headers as sent, the
  request and response bodies (JSON pretty-printed), time to first byte
  and total time, and **Copy as curl** to replay it.
- **HTTPS included.** The proxy is where TLS terminates, so it sees
  plaintext. Nothing in the container has to trust a second CA.
- **Nothing to add to the services.** No proxy variables, no middleware, no
  extra container in your config. A service you don't own, such as MinIO,
  is recorded the same as your own.
- **It keeps recording** across page reloads until you press **Stop**, so
  you can switch it on, reproduce a bug however long that takes, and come
  back to the whole sequence.

A recording lives in fghjd's memory only, never on disk, since it holds
cookies, tokens and bodies: the last 1,000 requests or 64 MiB per service.
You choose whether to keep text bodies (the default), all bodies up to
8 MiB, or headers only.

[Tutorial chapter 8](/tutorial/08-capturing-traffic/) follows one file
upload across the browser, nginx, the API and S3 this way.

## Making a call visible

Only calls to a service's **http-zone** name are proxied, so only those are
recorded. That's a choice you make per call in `.fghj.yaml`:

```yaml title=".fghj.yaml"
environment:
  # recorded: goes through the sidecar
  API_URL: http://${FGHJ_SERVICE_FQDN_HTTP:api}
  # not recorded: straight from container to container
  DATABASE_URL: postgres://app@${FGHJ_SERVICE_FQDN:db}:5432/app
```

Plain `http://` to an http-zone name is recorded exactly like `https://`,
so recording a call doesn't mean setting up TLS in the caller. For the
trade-offs per call, see [HTTP vs. raw](/guides/networking-http-vs-raw/).

## What it can't see

- **Non-HTTP protocols.** Postgres, Redis and Kafka go through the raw
  zone, with no proxy between the containers. Use tcpdump,
  or the database's own logging, for those.
- **Calls over a raw name.** An HTTP call to `${FGHJ_SERVICE_FQDN:api}:3000`
  is the shortest path and nobody is watching it. Switch the caller to the
  http name to see it.
- **Calls to the internet.** A request from your API to Stripe never touches
  a fghj proxy.

## Further reading

- [Tutorial chapter 8: Capturing traffic](/tutorial/08-capturing-traffic/)
- [Attaching a debugger](/guides/debugging/): when the request looks right
  and the service still gets it wrong.
- [In-network TLS proxy sidecar](/concepts/sidecar/): what sits in the
  middle and how it knows which container called.
