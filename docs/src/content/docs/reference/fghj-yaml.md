---
title: .fghj.yaml
description: Full reference for the .fghj.yaml file every repo declares — services, depends_on, include, and flows.
---

Every repo that participates in `fghj` carries its own `.fghj.yaml` at its
root. There's no shared/root config — each file is self-contained and
validated independently against fghj's CUE schema; see
[fghj validate](/cli/validate/). The full grammar lives in `schema/component.cue`
and `schema/dependency.cue` in the fghj repo.

The shape is Docker Compose's on purpose: `services:` with `build:` or
`image:`, and `depends_on:` between them. What fghj adds is `include:` (the
other repos this one talks to) and `flows:` (named start lists, so you don't
have to start everything at once).

## Top-level shape

```yaml
version: "2.0"
include:
  <alias>: <git url>
services:
  <service-name>:
    # ... see Services below
flows:
  <flow-name>: [<entry>, ...]
```

| Field | Type | Description |
|---|---|---|
| `version` | string matching `2.<minor>` | Which version of this language the file is written against. **Major is a compatibility barrier; minor is not.** A different major is refused outright — the file says something this build can't correctly interpret, and guessing would be worse than refusing. Any minor is accepted, *including one newer than the daemon knows*, paired with an advisory warning naming the repo and both versions. That asymmetry is what makes staggered adoption possible in a federated config: one repo can start using a `2.1` field while its peers stay on `2.0` and they still resolve into one graph. Without it, every repo in every workspace would have to change on the same day. |
| `include` | map of alias→git URL, or alias→`{repo, default_branch}` | The other repos this one talks to. See [`include`](#include). |
| `services` | map of name→`#Service` | Required. See [`services`](#services). |
| `flows` | map of name→list | See [`flows`](#flows). Any repo may declare flows — there's no distinguished root. |

## `services`

Everything that runs is a service, as in Compose: your own code (with
`build:`) and the things it needs (with `image:` — a database, a cache, a
broker). Exactly one of the two is required. The map key is the service's
name (lowercase, `[a-z0-9][a-z0-9-]*`); its node id is `{name}.{repo
folder}` — see [Node identity & domains](/concepts/node-identity-and-domains/).

| Declares | Shown as | Means |
|---|---|---|
| `build:` | service | Your code, built from this repo's checkout. |
| `image:` only | backing | A stock image. Nothing to build. |
| `run:`, or anything something requires with `condition: service_completed_successfully` | task | Runs to completion. Exit 0 is success. See [Tasks](#tasks). |

```yaml
services:
  cart-service:
    build:
      context: .
      dockerfile: Dockerfile
      args:
        NODE_ENV: production
    ports:
      "8080":
        primary: true
      "9090":
        name: admin
    domain_scope: run
    environment:
      - PORT=8080
    env_file:
      - .env
    platform: linux/arm64
    command: ["npm", "run", "dev"]
    restart: unless-stopped
    stop_signal: SIGQUIT
    stop_grace_period: 30
    user: "1000:1000"
    working_dir: /app
    labels:
      team: platform
    cap_add: ["NET_ADMIN"]
    extra_hosts:
      - "metadata:169.254.169.254"
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:8080/health"]
      interval: 10
      retries: 3
    depends_on:
      postgres: {condition: service_healthy}
      search: {required: false}
      auth/login: {}
  postgres:
    image: postgres:16
    healthcheck:
      test: ["CMD", "pg_isready"]
```

| Field | Type | Description |
|---|---|---|
| `build` | string or object | Builds this service from this repo's checkout, or from a git URL. The short form is the context (`build: .`), as in Compose. |
| `image` | string | Runs a stock image instead. Exactly one of `build` and `image`. |
| `build.context` | string | Docker build context: a path in this repo, defaulting to `.`, or a git URL `<url>#<ref>:<subdir>`. See [Building from a git URL](#building-from-a-git-url). |
| `build.dockerfile` | string | Dockerfile path, relative to `context`. Defaults to `Dockerfile`. |
| `build.dockerfile_inline` | string, optional | The Dockerfile itself, for a context that has none. Setting it and a `dockerfile` other than the default is a blocking warning. |
| `build.args` | map of string→string | Build-time `--build-arg` values. |
| `build.target` | string, optional | Which stage of a multi-stage Dockerfile to build — `docker build --target`. Omitted builds the final stage, as Docker does. |
| `build.ssh` | bool | Forwards the **workspace owner's** ssh-agent into the build as BuildKit's `default` socket, for a Dockerfile doing `RUN --mount=type=ssh` — cloning a private sibling repo, a private Go module, a private Cargo registry. The same agent fghj already forwards to `git clone`. Defaults to `false`. Note the credential comes from the person who owns the workspace, never from this config: a repo cannot ask for a key. |
| `build.secrets` | list of `{id, file}` | BuildKit secret mounts — `id` is what the Dockerfile names (`RUN --mount=type=secret,id=npmrc`), `file` is where the bytes come from, resolved against this repo's checkout root exactly like a bind mount's `host`. There is deliberately **no `env:` variant**, which BuildKit itself supports: `fghjd` is a root daemon with no access to your shell environment, so there'd be nothing to read one from. |
| `ports` | map of container-port→`#Port` | Declared container ports. The map key is the literal container port number (e.g. `"8080"`), published to Docker as-is — not a semantic label. See [Ports](#ports) below. |
| `domain_scope` | `"run"` \| `"stable"` | Whether this service's derived domain includes the run id. Defaults to `"run"`. See [Node identity & domains](/concepts/node-identity-and-domains/#domain-derivation-one-formula-no-exceptions-two-zones). |
| `environment` | map or list | Either `{KEY: value}` or a list of `"KEY=value"` strings — mirrors Docker Compose's own `environment` shape. Values can reference a sibling's domain with `${FGHJ_SERVICE_FQDN}`/`${FGHJ_SERVICE_FQDN_HTTP}` — see [Domain templates in `environment`](#domain-templates-in-environment) below. |
| `env_file` | list of strings | `.env`-style files loaded *before* `environment` — Compose's `env_file`. Each path resolves against this repo's own checkout root, same rule as `#Volume.host`. An explicit `environment` entry always wins over one loaded from a file. Resolves the same way for an `image:` service. Same `${FGHJ_SERVICE_FQDN}` templating as `environment` applies to loaded values too. |
| `platform` | string, optional | Pins the platform (`os[/arch[/variant]]`, e.g. `linux/arm64`) passed to `docker build --platform`, for cross-compiling this service's image to a specific architecture. Unset (the default) builds for the host's own platform. |
| `command` | list of strings | Overrides the image's default `CMD`, Compose-`command`-style. Empty (the default) leaves the image's own `CMD`/`ENTRYPOINT` untouched. |
| `restart` | `"no"` \| `"always"` \| `"on-failure"` \| `"unless-stopped"` | Compose-equivalent restart policy. Defaults to `"no"` — a stopped container stays stopped; `fghj daemon`'s own `ensure_running` is the usual way a container comes back, not Docker's own restart machinery. |
| `stop_signal` | string matching `SIG[A-Z0-9]+`, optional | The signal Docker sends to stop this container — Compose's `stop_signal`. Unset (the default) uses whatever the image declares via `STOPSIGNAL`, or SIGTERM. Override it only for an image whose process listens for something else (nginx's graceful "quit" is `SIGQUIT`). |
| `stop_grace_period` | non-negative integer (seconds) | How long Docker waits after the stop signal before following up with `SIGKILL`. Defaults to `10`, matching Docker's own. Raise it for anything that needs to finish writing before it dies — a database flushing to a `scope: stable` volume is the case this exists for. |
| `user` | string, optional | Overrides the image's default container user, e.g. `"1000:1000"` or `"postgres"`. |
| `working_dir` | string, optional | Overrides the image's default working directory. |
| `labels` | map of string→string | Extra container labels, merged under fghj's own `com.docker.compose.*` labels — fghj's own always win on a key conflict. |
| `cap_add` / `cap_drop` | list of strings | Linux capabilities to add/drop — Compose's `cap_add`/`cap_drop`. |
| `privileged` | bool | Runs the container with extended, near-host-equivalent privileges. Defaults to `false` — only set this for a real, specific need. |
| `extra_hosts` | list of `"hostname:ip"` strings | Extra literal entries written into *this container's own* `/etc/hosts` — Compose's `extra_hosts`. Distinct from `additional_hosts` below: this is the container resolving something else, not the host resolving this container. |
| `debug` | integer 1–65535, optional | The port your debugger listens on *inside* the container — 9229 for Node's inspector, 5678 for debugpy, 2345 for Delve. Declaring it publishes that port, exactly like `ports` — nothing is injected into the container's environment, because your image already knows which port its debugger listens on. Starting the debugger is your image's job; fghj deliberately knows nothing about which debugger this is. What `debug` adds over `ports` is that fghj knows which port is the debugger, so the UI can label the address and offer the per-container halt-at-startup switch (`FGHJ_DEBUG_WAIT`). See [Debugging in containers](/guides/debugging/). |
| `healthcheck` | `#Healthcheck`, optional | A Docker `HEALTHCHECK`. See [Healthcheck & start order](#healthcheck--start-order) below. |
| `volumes` | list of `#Volume` | Bind mounts and named volumes. See [Volumes](#volumes) below. |
| `additional_hosts` | list of `#HostAlias` | Extra literal hostname aliases this service also answers on, alongside its derived domain — each one optionally wildcarded to also match every subdomain of it. See [Additional hosts](#additional-hosts) below. |
| `depends_on` | list or map | What this service needs, to start or at runtime. See [`depends_on`](#depends_on) below. |
| `run` | `"on_start"` \| `"once"` | Makes this service a task. See [Tasks](#tasks). |

## Ports

The map key is the actual container port — the same number your app listens
on inside the container — not a semantic label. Give it a `name` if you want
a label too.

```yaml
ports:
  "8080":
    primary: true
  "9090":
    name: admin
  "9000":
    host_port: 9000
```

| Field | Type | Description |
|---|---|---|
| `primary` | bool | At most one port per service should set this. Puts the port at the service's own domain (`cart.myworkspace.fghj.internal`). Defaults to `false`. |
| `name` | string, optional | Gives the port an *additional* nested domain: `{name}.{service's domain}`. Can be combined with `primary`. |
| `host_port` | 1–65535, optional | Pin the host-side published port instead of letting Docker assign a random ephemeral one — for protocols whose clients hardcode a port and can't go through name-based routing at all. Only one run can hold this exact host port at a time. |
| `wildcard` | bool | When `primary` and/or `name` is set, also match every subdomain of this port's derived domain, not just the exact name — e.g. a `primary` port gets `*.cart.myworkspace.fghj.internal` too, not just `cart.myworkspace.fghj.internal` itself. No effect otherwise (a warning, not a hard failure, if set on a port that's neither). Defaults to `false`. |

A port with neither `primary` nor `name` is still published to an
ephemeral localhost port, just with no `*.fghj.internal` name.

## Domain templates in `environment`

`environment` (and `env_file`) values can reference a sibling's domain
without hand-computing it. Every node actually has two derived domains —
see [Node identity &
domains](/concepts/node-identity-and-domains/#domain-derivation-one-formula-no-exceptions-two-zones)
and [Split DNS](/concepts/split-dns/) — and the macro form you use picks
which one you get:

```yaml
environment:
  DATABASE_URL: postgres://user:pass@${FGHJ_SERVICE_FQDN:postgres}:5432/app
  PMA_ABSOLUTE_URI: https://${FGHJ_SERVICE_FQDN_HTTP}/
```

| Token | Resolves to |
|---|---|
| `${FGHJ_SERVICE_FQDN}` | This node's own **raw** domain (`*.fghj.raw.internal`) — direct, in-network-only, resolved straight to the real container IP. This is the default because it's what nearly every real caller needs: a database connection string, an internal API call, a raw port that isn't HTTP(S) at all. |
| `${FGHJ_SERVICE_FQDN:name}` | The **raw** domain of service `name` in this same repo. |
| `${FGHJ_SERVICE_FQDN:alias/name}` | The **raw** domain of service `name` in the repo included as `alias`. This is the one place a file may name another repo's service: an address is the network contract, and production config has it too. |
| `${FGHJ_SERVICE_FQDN_HTTP}` / `${FGHJ_SERVICE_FQDN_HTTP:…}` | The same lookups, but resolving to the **http** domain (`*.fghj.internal`) instead — proxied, TLS-terminated, the same address in or out of the run's docker network. Use this only when something specifically needs that proxied identity on purpose, e.g. minting a presigned URL meant to be handed to something outside the network. Reachable automatically from inside a container too — see [Reaching the TLS proxy from inside a container](#reaching-the-tls-proxy-from-inside-a-container) below. |

A reference is not an edge. It expands to a hostname, nothing more: it
never makes anything wait, never pulls anything into a run, and isn't drawn
in the graph. A service could just as well hardcode the hostname, and fghj
couldn't see that, so the graph doesn't depend on how an address happened to
be spelled. If a service needs another, say so in `depends_on`: required to
start, or `required: false` for one it only calls at runtime. Starting a run
that leaves out a `required: false` dependency of something it starts gets an
advisory ("'web.shop' needs 'billing/api' at runtime, but this run doesn't
start it").

A name that matches nothing — no such service, no such include, or no such
service in the included repo — is a blocking warning. A repo that isn't
cloned yet can't be checked, so its names pass until it is. You can't reach
a *named port* this way, only the service's bare domain.

## Volumes

Each entry is **either** a bind mount **or** a named volume, distinguished
by which key you set — `host` for a bind mount, `name` for a named volume.
Setting both, or neither, fails `fghj validate`.

Every container also gets one volume you didn't declare: `fghjd`
bind-mounts its CA trust files read-only at `/etc/fghj/certs` (see
[above](#reaching-the-tls-proxy-from-inside-a-container)). It is listed
before your own, so declaring something at that same path overrides it.

### Setting a bind mount

Use `host` when you want a path on your machine mounted straight into the
container — the everyday case is mounting your own live source over what
the image built, so edits on disk show up without a rebuild:

```yaml
volumes:
  - host: ./src
    container: /app/src
```

`host` resolves relative to *this repo's own checkout root* (not
`build.context`). It is **not sandboxed** to that repo, though — an
absolute path, or a `..`-escaping relative one, passes straight through to
Docker exactly as written, same as Compose. That's what lets you reach a
sibling repo checked out next to this one:

```yaml
volumes:
  - host: ../intel
    container: /app/intel
    read_only: true
```

| Field | Type | Description |
|---|---|---|
| `host` | string | A host path. Relative paths resolve against this repo's checkout root; absolute or `..`-escaping paths pass through unchanged. |
| `container` | string | Mount path inside the container. |
| `read_only` | bool | Mounts read-only. Defaults to `false`. |

### Setting a named volume

Use `name` instead of `host` when you want Docker-managed storage that
isn't tied to any host path — the everyday case is a Postgres whose data
needs to survive a container restart:

```yaml
volumes:
  - name: pgdata
    container: /var/lib/postgresql/data
```

`name` is a bare label, like `#Port.name` — the real Docker volume name is
*derived* from it, never the literal string you write. The derivation folds
in the workspace, the declaring node's id, and (depending on `scope`) the
run.

The node id in there is what makes the label **private to the node that
declared it**. `name: data` in one repo and `name: data` in another are two
different volumes, exactly as two services both called `api` are two
different nodes. This is deliberate: an unqualified volume namespace lets two
repos that each declare `{name: data, scope: stable}` for their own Postgres
end up with one volume and two engines writing to it — silent corruption,
produced by two individually valid configs written by teams who have never
spoken.

Sharing is still expressible, as an explicit opt-in on **both** sides:

```yaml
# service A's .fghj.yaml
volumes:
  - name: shared-cache
    container: /app/.cache
    shared: true

# service B's .fghj.yaml — same name, same scope, and shared on both sides
volumes:
  - name: shared-cache
    container: /var/cache/app
    shared: true
```

Reach for it rarely. The usual reason to want it — several services behind
one database — is already one service that the others `depends_on` (or,
across repos, a flow the database's repo publishes), which gives you one
*node*, and therefore one container and one volume, without any cross-repo
name coincidence being load-bearing. Note also that `shared: true` and
`shared: false` derive *different* names, so flipping it is a visible
migration rather than a silent adoption of somebody else's data.

| Field | Type | Description |
|---|---|---|
| `name` | string | A bare label, matching `[a-z0-9][a-z0-9-]*`. The real volume name is derived from it, folding in the declaring node's id — so the same label in two repos is two volumes unless both opt into `shared`. |
| `shared` | bool | Drops the node-id qualification so the label alone decides identity, letting any other node with the same `name` + `scope` + `shared: true` reach the same storage. Defaults to `false`. |
| `scope` | `"run"` \| `"stable"` | Same semantics as `domain_scope`: `"run"` (the default) gives each run (including named runs) its own fresh empty volume; `"stable"` gives the volume one fixed identity that persists across every run. |
| `container` | string | Mount path inside the container. |
| `read_only` | bool | Mounts read-only. Defaults to `false`. |

Stopping a `"run"`-scoped volume's named run deletes that volume
along with its containers and network — since `scope: "run"` under a named
run derives a run-specific volume name to begin with (folding the run id
in), there's nothing else that could still be using it once the run
stops. The default run and any `"stable"`-scoped volume are never deleted
this way: a `"stable"` volume's entire point is to persist across every
run, and the default run's own `"run"`-scoped volumes get the exact same
derived name on every start, so stopping and restarting the default run
must leave their data in place.

:::caution[`scope: "stable"` plus a second run]
`"stable"` means *one* volume, shared by every run — including two runs that
are up at the same time. Two Postgres containers (the default run's and a
named run's) mounting one `pgdata` is two engines on one data directory,
which Postgres does not survive gracefully. Nothing stops you: fghj neither
warns nor serialises access. Keep `scope: "stable"` for data that tolerates
concurrent readers, or accept that you'll run one run at a time for that
node. [Runs](/reference/runs/) walks through this and the
other two knobs that behave differently once a second run exists.
:::

## Additional hosts

A service is normally only reachable at its derived `*.fghj.internal`
domain (see [Node identity & domains](/concepts/node-identity-and-domains/)).
`additional_hosts` lets it also answer on one or more extra, literal
hostnames — useful when something outside fghj already has a hostname on
file, like a third-party OAuth callback pointing at `aikido.local`, and
reconfiguring that third party just to fit fghj's own domain isn't
practical. Each entry is a bare hostname (exact match only) or an object
with an explicit `wildcard` toggle:

```yaml
services:
  aikido-core:
    ports:
      "3000":
        primary: true
    additional_hosts:
      - aikido.local
      - app.local.aikido.io
      - host: myservice.local
        wildcard: true
```

Requires the service to have a `primary` port — declaring `additional_hosts`
without one is a warning (those hosts wouldn't be routed to anything), not
a hard `fghj validate` failure. Each alias is routed to the same port the
service's own derived domain uses.

Whether an alias gets HTTPS depends on its TLD:

- Under an IANA reserved special-use TLD — `.local`, `.test`, `.internal`,
  or `.localhost` (RFC 2606 / 6762, never delegated on the real internet) —
  it's issued a certificate from fghj's local CA, same as any
  `*.fghj.internal` name, but only while some running container actually
  claims it (`aikido.local` above).
- Anything else is treated as a real, potentially internet-routable
  hostname and is proxied over **plain HTTP only** — fghj's CA is a
  system-trusted root, and minting a certificate for a domain it doesn't
  own would be trusted by every app on the machine, not just fghj's own
  proxy (`app.local.aikido.io` above).

An alias can never sit inside `fghj.internal` itself — that domain is
always *derived*, never author-declared, the same rule `ports`' `name`
field follows.

### Wildcarding an alias

A bare entry (or `wildcard: false`, the default) only ever matches the
exact name you list — it can't help with a service that does
tenant-per-subdomain routing in production (`acme.myservice.org`,
`microsoft.myservice.org`, ..., arbitrarily many, not enumerable up front).
`wildcard: true` claims a whole DNS subtree instead: the entry matches its
own apex *and* any subdomain of it, however deep, including ones you never
listed:

```yaml
services:
  myservice:
    ports:
      "8080":
        primary: true
    additional_hosts:
      - host: myservice.local
        wildcard: true
```

With this, `acme.myservice.local`, `microsoft.myservice.local`, and any
other `*.myservice.local` all route to `myservice`'s primary port — the
proxy forwards the raw `Host` header/SNI untouched, so the app's own
tenant-resolution logic runs exactly as it does in production.

The reserved-TLD rule for HTTPS still applies per-name (a real cert is
minted lazily for each exact subdomain actually requested, never a literal
X.509 wildcard cert). `fghj validate` also warns if two different nodes
declare the same wildcarded suffix — unlike a plain exact-match collision,
this claims traffic for a whole subtree of names, not just one, and is
otherwise only discoverable by noticing traffic silently going to the
wrong container.

Unlike an exact-match alias, which works by writing an entry into
`/etc/hosts`, a wildcarded one is resolved by fghjd's own DNS server
(`/etc/hosts` has no wildcard syntax) — see
[Split DNS](/concepts/split-dns/).

### Wildcarding the default domain

`additional_hosts` can't name anything inside `fghj.internal` — that
domain is always derived, never author-declared. To wildcard a node's own
default domain instead (so it, too, matches every subdomain of itself, not
just the exact name), set `wildcard: true` on the `#Port` that's `primary`
and/or `name`d — see [Ports](#ports) above.

## Reaching the TLS proxy from inside a container

Every node actually has two derived domains — a raw one
(`*.fghj.raw.internal`) and an HTTP(S)-proxied one (`*.fghj.internal`) —
covered in full in [Node identity &
domains](/concepts/node-identity-and-domains/#domain-derivation-one-formula-no-exceptions-two-zones)
and [Split DNS](/concepts/split-dns/). From inside a service's own
container, a sibling's **raw** domain resolves straight to that sibling's
container IP via Docker's own embedded per-network DNS — the right
behavior for a node's own container port (a database's `5432`, an internal
API's own port): fastest path, no extra hop, no TLS.

A sibling's **http** domain, from inside the same container, resolves
instead to this run's TLS proxy sidecar — the exact same SNI-dispatch
logic, same hostname, same port (`443`/`80`), that a browser or host
process outside the network gets when it dials that name. This is
automatic for every node in a run: nothing to declare, no `extra_hosts`
entry, no sentinel. Use the http domain (via
`${FGHJ_SERVICE_FQDN_HTTP:name}` — see [Domain templates in
`environment`](#domain-templates-in-environment) above) for anything that
specifically needs the *same* HTTPS hostname a browser or host process
would use — most commonly, a service that mints URLs meant to be handed
back out (a presigned S3 URL, an OAuth redirect, a webhook callback) and
can only have one endpoint configured for both its own calls and the URLs
it generates.

The container also needs to trust fghj's local CA for that TLS connection
to succeed. The cert and a ready-made full trust bundle are already mounted
read-only in every container at `/etc/fghj/certs/`, so this is usually one
line:

```yaml
environment:
  SSL_CERT_FILE: /etc/fghj/certs/bundle.pem
```

See [Trusting fghj's CA inside a
container](/guides/networking-http-vs-raw/#trusting-fghjs-ca-inside-a-container)
for which of the two files to use and why, including Node and Java.

This mechanism doesn't depend on any container-runtime-specific gateway
forwarding — it works the same way on Docker Desktop, OrbStack, and native
Linux Docker Engine alike.

## Healthcheck & start order

```yaml
services:
  api:
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:8080/health"]
      interval: 10
      timeout: 5
      start_period: 30
      retries: 3
```

| Field | Type | Description |
|---|---|---|
| `test` | non-empty list of strings | The command Docker runs to check health, e.g. `["CMD", "pg_isready"]` — same shape as Docker's own `HEALTHCHECK CMD`. Required, and it must have at least one element: Docker reads an empty `test` as "inherit whatever the image declares", so `test: []` would silently leave the node with no healthcheck of its own. Writing it earns a blocking warning rather than that silence. |
| `interval` / `timeout` / `start_period` | seconds, optional | Same semantics as Docker's `HEALTHCHECK` options of the same name (given in seconds here, not nanoseconds). |
| `retries` | integer, optional | Consecutive failures before Docker marks the container `unhealthy`. |

A node that something in the run can't start without is waited on before
its dependents start: until Docker reports it `healthy` (up to two minutes)
if it declares a `healthcheck`, and as soon as it has started otherwise. A
[task](#tasks) is always waited on until it exits. A node nothing in the run
requires isn't waited on at all, healthcheck or not, so it never holds up the
rest of the run. `condition` in `depends_on` doesn't change how long anything
waits; it states what the dependent expects, and fghj refuses a start where
that isn't true (see [`depends_on`](#depends_on)). Containers within a run
start in the order of their required `depends_on` edges, not workspace-scan
order.

## `depends_on`

Compose's syntax. Every edge between two services is declared here, and
each entry is one of two kinds: **needed to start** (`required: true`, the
default) or **needed at runtime** (`required: false`).

```yaml
depends_on: [postgres, redis]          # short form

depends_on:                            # long form
  postgres: {condition: service_healthy}
  migrate: {condition: service_completed_successfully}
  search: {required: false}
  billing/db: {condition: service_healthy}
  billing: {}
```

| Field | Description |
|---|---|
| `condition` | `service_started` (the default, as in Compose), `service_healthy` or `service_completed_successfully`. `service_healthy` needs the target to declare a `healthcheck`. `service_completed_successfully` makes the target a [task](#tasks), and then everything that waits on it has to wait that way. A mismatch is a blocking warning. |
| `required` | Defaults to `true`. See the table below. |

| | `required: true` (default): needed to start | `required: false`: needed at runtime |
|---|---|---|
| Graph | solid line | dashed line |
| Which runs start it | every run that starts this service | only a run whose flows list it |
| Order and waiting | starts first; this service waits until it's ready | none |
| It fails to start | this service isn't started: "blocked by …", transitively | nothing |
| It's recreated or stopped | this service is restarted or stopped with it | nothing |
| A cycle | blocking: none of them can start | can't happen |
| Not in the run | can't happen | an advisory before start |

Ask one question: **will this service crash, or start broken, if that one
isn't there when it boots?** A database the connection pool opens at startup
is required. A webhook receiver nobody calls until a user clicks something
isn't. `condition: service_completed_successfully` with `required: false` is
a blocking warning, since "must have finished" only makes sense before a
start; any other condition on a `required: false` entry is ignored, with an
advisory.

A `${FGHJ_SERVICE_FQDN:…}` hostname isn't an edge. It must name an existing
service, but it orders, waits on and starts nothing: declare the dependency
in `depends_on` if there is one. See
[Dependency kinds](/concepts/dependency-kinds/).

What an entry can name:

| Entry | Means |
|---|---|
| `postgres` | A service in this repo. |
| `billing/db` | A **flow** of the repo included as `billing`: waits on every member of that flow, and starting this service starts them. |
| `billing` | All of the repo included as `billing` — its own services. |

Across repos, `depends_on` only ever names flows, never services: another
repo's services are its internals, and its flows are what it publishes.
`billing/api` where `api` is a service is a blocking warning that lists the
flows `billing` does publish. If it publishes none, name `billing` and get
all of it — and ask its team to publish something smaller. Waiting on another
repo with `service_completed_successfully` is refused too: a flow is waited
on until it's up, and it never completes.

`depends_on` describes production, so it's the same in every run. A cycle
of required entries is a blocking warning: each service waits for the next,
so none of them can start. Services that call each other at runtime mark one
side, or both, `required: false`.

## Building from a git URL

To run someone else's code with your own definition (an open-source service
that ships no Dockerfile, say), give `build.context` a git URL, Compose's
way:

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
      "8080": {primary: true}
```

- **`<url>#<ref>:<subdir>`.** `ref` is a branch, tag or commit; without it,
  the remote's default branch. `subdir` is optional and must stay inside the
  clone. A context is a URL when it starts with `https://`, `http://`,
  `ssh://`, `git@` or `file://`.
- **The service is yours.** Its id, domain, flows and `depends_on` are this
  repo's. The URL's own `.fghj.yaml`, if it has one, means nothing here; use
  [`include`](#include) for that.
- **Pull clones it, start never does.** The clone goes to
  `<workspace>/.fghj/sources/<name>@<ref>`, made by Pull all, a flow's pull or
  the node's Pull button, as the workspace owner. Starting a service whose
  source isn't pulled fails with "source not pulled". An existing clone is
  never fetched again: change the ref to move to a new version.
- **The clone is a checkout.** Its HEAD drives drift, and the Drawer shows it
  with its dirty state, so you can patch the code there while debugging. Its
  `.git` is never sent to the build, and an inline Dockerfile travels in the
  build context as `.fghj.Dockerfile`, so nothing is written into the clone.
  The image is tagged `fghj/<id>:<ref>`.

## `include`

The other repos this one talks to, by alias:

```yaml
include:
  billing: git@github.com:acme/billing.git
  auth:
    repo: git@github.com:acme/auth-service.git
    default_branch: develop
```

The alias is what `depends_on`, `flows` and hostnames use (`billing/db`,
`${FGHJ_SERVICE_FQDN:billing/api}`). It can be anything, as long as it isn't
also the name of a service or flow in this file.

fghj finds the repo by its git remote, so the folder it's checked out in can
be called anything. A repo that isn't cloned yet shows up as one stub node
and is cloned on pull. `default_branch` is the branch fghj clones it at the
first time — `git clone --branch <it> --single-branch`, defaulting to `main`.
It is read once, at clone time: re-pulling a checkout that already exists
never moves it off the branch you're on, so this is a starting point and
never a live pin — see [Branch ownership model](/concepts/branch-ownership-model/).

Including a repo means: when nothing narrower is asked for, start all of
it too, as production does.

## Tasks

A task is a container that runs **to completion** and then stays exited —
a migration, a seed, a fixture loader. It's an ordinary service; it's a task
when it declares `run:`, or when something requires it with
`condition: service_completed_successfully`:

```yaml
services:
  api:
    build: .
    depends_on:
      migrate: {condition: service_completed_successfully}
  migrate:
    build: .                      # the same code, another command
    command: ["./bin/migrate"]
    depends_on:
      db: {condition: service_healthy}
  db:
    image: postgres:16
    healthcheck:
      test: ["CMD", "pg_isready"]
```

`exited` means opposite things for a service and a task — drift for one,
success for the other — which is why fghj has to know. See
[Terminating nodes](/concepts/terminating-nodes/) for the full argument, and
[tutorial chapter 3](/tutorial/03-a-migration/) for a worked example.

| Field | Description |
|---|---|
| `command` | **Required, and non-empty** — a task *is* its command. Without one it would run the image's default `CMD`, which for your own image is usually the long-running server: it would never exit, and the run would hang until the task budget expired. |
| `run` | Declaring it makes the service a task. `"on_start"` (the default) re-runs on every start and every top-up, which is what a migration wants — its command is expected to be idempotent. `"once"` runs it at most once per run, for the expensive or destructive case. See the note below on what `once` costs. |
| `restart` | Not allowed — a restart policy on a container whose whole purpose is to exit would restart it forever. |
| `healthcheck` | Not allowed — an exited container can never report Docker-`healthy`. |

**Ordering is load-bearing, not decoration.** A task isn't considered
*started* until it has **finished**: fghj waits for the container to exit, and
a task that exits non-zero — or never exits — fails the node. A failed node
blocks everything that requires it, transitively, so a failed migration keeps
the service from coming up against an unmigrated database. Everything else in
the run keeps starting, and the run then reports what failed and what was
blocked. Above, the start order is `db → migrate → api`.

A task only one journey needs (a seed) declares `run: on_start` and its own
`depends_on` (on the database it seeds), and is listed in that journey's flow.
Nothing waits on it; it starts once what it needs is up.

```yaml
services:
  seed:
    build: .
    command: ["./bin/seed"]
    run: on_start
    depends_on:
      db: {condition: service_healthy}
flows:
  demo: [api, seed]
```

:::caution[What `run: once` actually means]
"At most once per run" — full stop. It is **not** re-run when your code
changes. A `git pull` that adds a migration will not cause a `once` task to
run again, even though fghj can see the commit moved. That's deliberate: an
author reaches for `once` exactly when re-running is expensive or destructive,
and "new code arrived" is not a reason to re-run something marked unsafe to
re-run. `once` does mean "succeed once", though — a task that exited non-zero
is never treated as done, so a failure is always retryable.
:::

## `flows`

A flow is a named, public start list. Starting nothing in particular starts
everything — every service here and in every included repo, as production
does. A flow exists so you don't have to.

```yaml
flows:
  checkout: [web, billing/pricing]
  admin: [web, admin-api, checkout]
  everything-billing: [web, billing]
```

| Entry | Means |
|---|---|
| `web` | A service in this repo. |
| `checkout` | Another flow in this repo. |
| `billing/pricing` | A flow of the repo included as `billing`. |
| `billing` | All of that repo, and everything it includes. |

A flow's id is `{repo folder}/{flow}` — `shop/checkout` — so two repos can
both have a `checkout`. What starting it starts is its entries, expanded,
plus everything they can't start without (`depends_on` entries that aren't
`required: false`, followed across repos). Flows may reference each other,
even in a circle; each is expanded once.

As with `depends_on`, a flow never names another repo's services. An entry
that names nothing is a blocking warning, reported once, by the flow it's
written in. A flow may be empty (`nothing: []`); it still exists, so naming
it isn't a typo.

**Who writes what.** A repo publishes flows for the slices of itself that
make sense on their own (`pricing`, `db`). A repo that uses another names
those flows. Neither has to know the other's internals, and the cost of a
repo publishing too little is starting too much — slow, never broken.
