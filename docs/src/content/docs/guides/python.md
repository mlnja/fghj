---
title: "Guide: Python services in fghj"
description: A Python service in fghj from scratch — the Dockerfile, the port, a healthcheck without curl, a Postgres it owns, Alembic as a task, live reload, and debugpy.
---

Nothing in fghj is Python-aware, and this guide is mostly ordinary Docker
advice with the fghj-shaped parts called out. There are four places where
Python specifically trips people up here:

- **Binding to `localhost`**, which means nothing outside the container.
- **Buffered stdout**, which makes a working service look silent.
- **`healthcheck` with no `curl`**, because the `slim` images don't ship one.
- **Absolute URLs**, because fghj's proxy terminates TLS and sends plain
  HTTP to your port with no `X-Forwarded-*` headers.

Everything else is the same `.fghj.yaml` any other runtime writes.

## A service, end to end

```docker title="Dockerfile"
FROM python:3.13-slim

# Dependencies in their own layer, before the source: editing a .py file
# then doesn't re-run pip.
WORKDIR /app
COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

COPY app ./app

# Without this, print() and logging go into a pipe buffer and you see
# nothing in the UI's log pane until the process exits or writes 8 KiB.
# This single line is the most common fix for "my container produces no
# logs".
ENV PYTHONUNBUFFERED=1

# --host 0.0.0.0, always. Uvicorn's default is 127.0.0.1, which inside a
# container means the loopback of that container and nothing else: the
# published port connects and then gets nothing.
CMD ["uvicorn", "app.main:app", "--host", "0.0.0.0", "--port", "8000"]
```

```yaml title=".fghj.yaml"
version: "1.0"

services:
  api:
    build:
      context: .
    ports:
      "8000":
        primary: true
    environment:
      OWN_URL: https://${FGHJ_SERVICE_FQDN_HTTP}
    healthcheck:
      test: ["CMD", "python", "-c", "import urllib.request; urllib.request.urlopen('http://localhost:8000/health')"]
      interval: 2
      retries: 15
```

That's a service answering at `https://api.<repo>.<workspace>.fghj.internal`
with real HTTPS. Note that `ports` is keyed by the number your app actually
listens on — it's the container port, not a label.

### The healthcheck, without curl

`python:3.13-slim` has no `curl` and no `wget`. Rather than installing one
just to poll yourself, use the interpreter you already have:

```yaml
healthcheck:
  test: ["CMD", "python", "-c", "import urllib.request; urllib.request.urlopen('http://localhost:8000/health')"]
```

`urlopen` raises on a non-2xx status, which exits non-zero, which is exactly
what Docker reads as unhealthy. No `sys.exit` bookkeeping needed.

A node that declares a `healthcheck` is automatically waited on: anything
that depends on it doesn't start until Docker reports it `healthy`. There is
no `depends_on: {condition: ...}` to write — declaring the check *is* the
declaration.

### Absolute URLs

The proxy terminates TLS on the host and relays plain HTTP to your
container's port. It does **not** add `X-Forwarded-Proto` or
`X-Forwarded-Host`, so there is nothing for `--proxy-headers` to read and no
way for your app to infer the scheme from the request. If you mint absolute
URLs — an OAuth redirect, a password-reset link, a presigned URL — read them
from the environment instead:

```yaml
environment:
  OWN_URL: https://${FGHJ_SERVICE_FQDN_HTTP}
```

fghj expands that when the container is created. Your code never computes
its own hostname, which is right anyway: the hostname depends on the
workspace directory's name and the run, neither of which the code can know.

## A database it owns

A `kind: backing` dependency is an image this service provisions for
itself. The declaring service owns it; nothing else can see it unless it
asks by name.

```yaml
    environment:
      DATABASE_URL: postgresql://app:dev@${FGHJ_SERVICE_FQDN:db}:5432/app
    dependencies:
      - kind: backing
        name: db
        image: postgres:16
        environment:
          POSTGRES_USER: app
          POSTGRES_PASSWORD: dev
          POSTGRES_DB: app
        ports: ["5432"]
        healthcheck:
          test: ["CMD-SHELL", "pg_isready -U app"]
          interval: 2
          retries: 15
        stop_grace_period: 30
        volumes:
          - name: pgdata
            scope: stable
            container: /var/lib/postgresql/data
```

`${FGHJ_SERVICE_FQDN:db}` expands to that container's raw domain — direct to
the container IP, no proxy, no TLS, on the real port. It's the right
address for a connection string and for every service-to-service call that
isn't specifically about the proxied HTTPS identity.

Use `psycopg[binary]` in `requirements.txt` rather than plain `psycopg`:
the binary wheel carries its own libpq, so the image needs no
`libpq-dev`/`gcc` and the build stays three lines.

## Migrations as a task

A migration is a container that's *supposed* to exit, which is a different
node kind rather than a flag:

```yaml
      - kind: task
        name: migrate
        command: ["alembic", "upgrade", "head"]
        after: ["db"]
        environment:
          DATABASE_URL: postgresql://app:dev@${FGHJ_SERVICE_FQDN:db}:5432/app
```

With no `image:`, the task runs **the owning service's own built image** with
a different command — which is what a migration almost always wants, since
it's your code and your `alembic/` directory. `after: ["db"]` orders it
behind the database, and because `db` declares a healthcheck, "behind" means
after Postgres is actually accepting connections.

A task isn't considered started until it has *finished*, and a non-zero exit
fails the node and blocks everything downstream. A migration that fails
therefore stops the service coming up against an unmigrated database,
instead of racing it.

The default `run: on_start` re-runs the task on every start and every
top-up, so the command must be idempotent — `alembic upgrade head` is.

## Live reload

Mount your source over what the image built and let uvicorn watch it:

```yaml
    command: ["uvicorn", "app.main:app", "--host", "0.0.0.0", "--port", "8000", "--reload"]
    volumes:
      - host: ./app
        container: /app/app
```

Mount the **source directory only**, never `/app` itself: a bind mount at
`/app` would also shadow anything the image put there, and if you ever
install your project into the image (`pip install -e .`) it will shadow the
egg-link too. Relative `host` paths resolve against this repo's checkout
root.

Add `__pycache__` and `.venv` to `.dockerignore` while you're there — a
local virtualenv copied into the build context is both slow and, if its
paths leak, wrong.

## Debugging

`debug:` publishes a port and nothing else; starting the debugger is the
image's job. The idiomatic hook in Python is `sitecustomize.py`, which
every Python process imports at startup — so it survives `gunicorn`, forks,
and shell wrappers without having to be PID 1:

```docker
RUN pip install --no-cache-dir debugpy
COPY sitecustomize.py /usr/local/lib/fghj/
ENV PYTHONPATH=/usr/local/lib/fghj
```

```python title="sitecustomize.py"
import os

# One process has to own the port. Setting our own marker means forked or
# re-exec'd children inherit it and skip — the kind of variable worth
# having, and it's yours: fghj injects nothing here.
if not os.environ.get("APP_DEBUGPY_LISTENING"):
    os.environ["APP_DEBUGPY_LISTENING"] = "1"
    import debugpy

    debugpy.listen(("0.0.0.0", 5678))
    if os.environ.get("FGHJ_DEBUG_WAIT"):
        debugpy.wait_for_client()
```

```yaml
    debug: 5678
```

Attach to `api.<repo>.<workspace>.fghj.raw.internal:5678`. Listening costs
essentially nothing and can't stop anything, which is why it isn't gated —
a breakpoint doesn't exist until an IDE has attached and sent one.
`FGHJ_DEBUG_WAIT` is the per-container switch in the UI for the one case
attaching can't reach: code that runs during startup.

Two caveats specific to this hook: `python -S` and `python -E` bypass it
entirely (`-S` skips `site`, `-E` ignores `PYTHONPATH`), and PyCharm's
remote debugger speaks to its own `pydevd-pycharm` package rather than
`debugpy` — same idea, different pip install.

[Attaching a debugger](/guides/debugging/) has the whole contract, and
[tutorial chapter 7](/tutorial/07-attaching-a-debugger/) walks through a
Python breakpoint catching a request mid-flight.

## Gotchas, collected

| Symptom | Cause |
|---|---|
| Port connects, nothing answers | Bound to `127.0.0.1`. Use `--host 0.0.0.0`. |
| No logs at all | Missing `PYTHONUNBUFFERED=1` (or `python -u`). |
| Healthcheck always fails | No `curl` in the `slim` image — poll with `python -c`. |
| Redirects go to `http://` | Nothing sets `X-Forwarded-Proto`. Read `${FGHJ_SERVICE_FQDN_HTTP}` from the environment. |
| `pip install` on every edit | `COPY requirements.txt` before `COPY app`. |
| Debugger port already in use | Several Python processes ran `sitecustomize.py`. Use the marker variable above. |
