---
title: 8. Capturing traffic
description: Recording what the services say to each other — an upload followed across the browser, nginx, the api and S3, what the Traffic tab keeps, and what it can't see.
sidebar:
  order: 8
---

Your browser's DevTools show what the browser sent. In this app that's
the smaller half. When you upload a file, the browser talks to nginx, nginx
to the api, the browser to S3, and the api to S3 again, and only two of
those four calls start in the browser.

fghj can show all four, because it's already in the path. Every call to a
`*.fghj.internal` name goes through one of its proxies: the one on your
host for the browser, the workspace's sidecar for a container. Either one
can record what it forwards.

Nothing to check out this time. This chapter changes no file in any repo;
everything it uses has been there since chapter 4.

## Record an upload

Open `https://files.test` and log in. Then, in fghj's UI:

1. Click `api` in the graph, open its **Traffic** tab and press **⏺ Record**.
2. Click `s3`, open its **Traffic** tab and press **⏺ Record** there too.

Both cards now have a pulsing red dot. Upload a small image, and look at
`api`'s tab:

```
POST  200  /api/files                web.files-web
POST  200  /api/files/…/complete     web.files-web
```

and at `s3`'s:

```
PUT   200  /files/uploads/…          host
HEAD  200  /files/uploads/…          api.files-api
```

That's the whole upload, in order:

1. The browser asks the api for somewhere to put the file. The api writes a
   row and answers with a presigned `uploadUrl`.
2. The browser `PUT`s the bytes straight to S3 at that URL. The api never
   handles them.
3. The browser tells the api it's done.
4. The api checks the object really landed, with a `HEAD` to S3, before it
   marks the file ready and queues a thumbnail.

The last column is the **source**, who made the call. `web.files-web` and
`api.files-api` are containers: the sidecar looked up which node the
connection came from. `host` is anything on your machine, here the browser.
The source names above the list are filters; click one to hide its rows.

Click a row to open it. **Headers** has both sides' headers as they went
over the wire, **Request** and **Response** have the bodies, JSON
pretty-printed, and **Timing** has the time to first byte and the total.
**Copy as curl** puts the request on your clipboard, ready to replay.

## It keeps recording

Close the drawer, reload the page, open another node: the recording goes
on. It belongs to fghjd, not to the page, so you can switch it on, go and
reproduce the bug however long that takes, and come back to find it all
there. Only **Stop** ends it, or fghjd restarting. The red dot on the card
is there so you don't forget one.

**Clear** empties the list and keeps recording.

Nothing is written to disk. A recording holds session cookies, bearer
tokens and request bodies, and it lives in fghjd's memory only, up to the
last 1,000 requests or 64 MiB per service, whichever comes first.

## What bodies are kept

The menu next to **Record** picks which bodies are kept:

- **Text bodies**, the default: JSON, text, XML, forms. Anything binary is
  recorded by type and size only.
- **All bodies**: binary too, up to 8 MiB each.
- **Headers only**: no bodies at all.

That's why the `PUT` in `s3`'s list has no body you can open. Switch `s3`
to **All bodies**, upload the image again, and the new `PUT`'s **Request**
tab shows the picture. Headers are always kept, whichever you pick.

## What it can't see

Press **⏺ Record** on `db`. The tab says it's recording, and it stays
empty however much you click. Postgres isn't HTTP, and the api reaches it
at `db.files-api.files.fghj.raw.internal:5432`, the **raw** zone from
chapter 2: straight from one container to the other, with no proxy in
between to watch it.

The same goes for any HTTP call made over a raw name. The worker reaches
the api at `${FGHJ_SERVICE_FQDN:api/api}:3000`, so its "thumbnail ready"
calls never show up in `api`'s tab. That's the trade the raw zone makes:
the shortest path, and nobody looking.

To make a call visible, give the caller the http name instead.
`files-web` did exactly that in chapter 4, and it's why nginx's calls are
in `api`'s list at all. It doesn't need TLS for that: nginx calls
`http://`, and plain HTTP through the sidecar is recorded the same as
HTTPS. See [Reaching the TLS proxy from inside a
container](/reference/fghj-yaml/#reaching-the-tls-proxy-from-inside-a-container)
for the two things that can go wrong.

When you're done, press **Stop** on each card that has a red dot.

---

That's the tutorial. From here:

- [Attaching a debugger](/guides/debugging/): the contract in one page,
  plus recipes for Python, the JVM, Ruby, Go and PHP/Xdebug (which is the
  exception: it dials out to your IDE, so `debug:` is the wrong tool).
- [Choosing HTTP or raw](/guides/networking-http-vs-raw/): which zone each
  kind of call wants.
- [.fghj.yaml reference](/reference/fghj-yaml/): the page to keep open.
- [Concepts](/concepts/architecture/): one page per subsystem, each
  explaining why it is the way it is.
