---
title: .fghj.yaml
description: Full reference for the .fghj.yaml file every repo declares — services, ports, dependencies, and flows.
---

Every repo that participates in `fghj` carries its own `.fghj.yaml` at its
root. There's no shared/root config — each file is self-contained and
validated independently against fghj's CUE schema; see
[fghj validate](/cli/validate/). The full grammar lives in `schema/component.cue`
and `schema/dependency.cue` in the fghj repo.

## Top-level shape

```yaml
version: "1.0"
services:
  <service-name>:
    # ... see Services below
flows:
  <flow-name>:
    # ... see Flows below
```

| Field | Type | Description |
|---|---|---|
| `version` | string matching `1.<minor>` | Which version of this language the file is written against. **Major is a compatibility barrier; minor is not.** A different major is refused outright — the file says something this build can't correctly interpret, and guessing would be worse than refusing. Any minor is accepted, *including one newer than the daemon knows*, paired with an advisory warning naming the repo and both versions. That asymmetry is what makes staggered adoption possible in a federated config: one repo can start using a `1.1` field while its peers stay on `1.0` and they still resolve into one graph. Without it, every repo in every workspace would have to change on the same day. |
| `services` | map of name→`#Service` | See [`services`](#services). At least one service or one flow makes the file useful; neither is structurally required. |
| `flows` | map of name→`#Flow` | See [`flows`](#flows). Any repo may declare flows — there's no distinguished root. |

## `services`

Keyed by service name — a repo can declare more than one independently
buildable service, each with its own Dockerfile and dependencies (e.g. a
dev-server process and a backend API process built from the same repo). The
map key *is* the service's name (lowercase, `[a-z0-9][a-z0-9-]*`) — it's a
human-readable label, not the node's internal id; see
[Node identity & domains](/concepts/node-identity-and-domains/) for why
those differ. Most repos declare exactly one.

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
    dependencies:
      - kind: service
        repo: git@github.com:acme/auth-service.git
        default_branch: main
```

| Field | Type | Description |
|---|---|---|
| `build.context` | string | Docker build context. Defaults to `.`. |
| `build.dockerfile` | string | Dockerfile path, relative to `context`. Defaults to `Dockerfile`. |
| `build.args` | map of string→string | Build-time `--build-arg` values. |
| `build.target` | string, optional | Which stage of a multi-stage Dockerfile to build — `docker build --target`. Omitted builds the final stage, as Docker does. |
| `build.ssh` | bool | Forwards the **workspace owner's** ssh-agent into the build as BuildKit's `default` socket, for a Dockerfile doing `RUN --mount=type=ssh` — cloning a private sibling repo, a private Go module, a private Cargo registry. The same agent fghj already forwards to `git clone`. Defaults to `false`. Note the credential comes from the person who owns the workspace, never from this config: a repo cannot ask for a key. |
| `build.secrets` | list of `{id, file}` | BuildKit secret mounts — `id` is what the Dockerfile names (`RUN --mount=type=secret,id=npmrc`), `file` is where the bytes come from, resolved against this repo's checkout root exactly like a bind mount's `host`. There is deliberately **no `env:` variant**, which BuildKit itself supports: `fghjd` is a root daemon with no access to your shell environment, so there'd be nothing to read one from. |
| `ports` | map of container-port→`#Port` | Declared container ports. The map key is the literal container port number (e.g. `"8080"`), published to Docker as-is — not a semantic label. See [Ports](#ports) below. |
| `domain_scope` | `"run"` \| `"stable"` | Whether this service's derived domain includes the run id. Defaults to `"run"`. See [Node identity & domains](/concepts/node-identity-and-domains/#domain-derivation-one-formula-no-exceptions-two-zones). |
| `environment` | map or list | Either `{KEY: value}` or a list of `"KEY=value"` strings — mirrors Docker Compose's own `environment` shape. Values can reference a sibling's domain with `${FGHJ_SERVICE_FQDN}`/`${FGHJ_SERVICE_FQDN_HTTP}` — see [Domain templates in `environment`](#domain-templates-in-environment) below. |
| `env_file` | list of strings | `.env`-style files loaded *before* `environment` — Compose's `env_file`. Each path resolves against this repo's own checkout root, same rule as `#Volume.host`. An explicit `environment` entry always wins over one loaded from a file. Also available on `kind: backing` — see its own field table below for how the path resolves there. Same `${FGHJ_SERVICE_FQDN}` templating as `environment` applies to loaded values too. |
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
| `dependencies` | list of `#Dependency` | This service's baseline dependencies — always pulled in regardless of which flow is selected. See [Dependencies](#dependencies) below. |

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
| `${FGHJ_SERVICE_FQDN:name}` | The **raw** domain of whichever sibling is declared with that leaf `name`, same lookup rules as below. |
| `${FGHJ_SERVICE_FQDN_HTTP}` / `${FGHJ_SERVICE_FQDN_HTTP:name}` | The same lookups, but resolving to the **http** domain (`*.fghj.internal`) instead — proxied, TLS-terminated, the same address in or out of the run's docker network. Use this only when something specifically needs that proxied identity on purpose, e.g. minting a presigned URL meant to be handed to something outside the network. Reachable automatically from inside a container too — see [Reaching the TLS proxy from inside a container](#reaching-the-tls-proxy-from-inside-a-container) below. |
| `${FGHJ_SERVICE_FQDN:a::b::name}` / `${FGHJ_SERVICE_FQDN_HTTP:a::b::name}` | Same lookup, but disambiguates a leaf `name` that matches more than one sibling by also qualifying it with as many of its owning segments as needed, root-first — the same segments a node's id is built from leaf-first (`{name}.{owner-id}`), just written in the opposite, more-readable order. E.g. `aikifactory::aikifactory::minio` reaches the same node as the id `minio.aikifactory.aikifactory` (before the workspace suffix). |

The bare `name` lookup (with or without the `_HTTP` suffix) resolves to
whichever sibling is declared with that leaf `name`: a `kind: backing`
dependency owned by the same service as the node whose `environment` this
is (a backing dependency can reference another backing dependency this way
too, not just the owning service), or — if no such backing dependency
matches — a service this node directly depends on via `kind: service`
(same-repo or cross-repo).

Can't reach a *named port* on a sibling — only its bare domain — and for
the `kind: service` case, only a dependency this exact node declares
itself, not a transitively-reached one. A malformed token (missing `}`) or
a path that doesn't resolve to any sibling is left in the output as literal
text rather than failing the run, so a typo is diagnosable from the
container's own env instead of silently swallowed.

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
isn't tied to any host path — the everyday case is a `kind: backing`
Postgres whose data needs to survive a container restart:

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
one database — is already `kind: shared-backing`, which gives you one *node*,
and therefore one container and one volume, without any cross-repo name
coincidence being load-bearing. Note also that `shared: true` and
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

There's no separate `depends_on: {condition: ...}` field — a node that
declares `healthcheck` is automatically waited on: any run that starts it
blocks (up to two minutes) until Docker reports it `healthy` before moving
on to the nodes that depend on it. A node with no `healthcheck` behaves
exactly as before — dependents proceed as soon as it's started, not
waiting on anything. This applies both to starting a named run from scratch
and to topping up the default environment for a flow; containers within a
run always start in dependency order (`depends-on`/`owns` edges), not
workspace-scan order.

## Dependencies

Four kinds, distinguished by `kind`:

| `kind` | What it is |
|---|---|
| [`service`](#kind-service) | Another self-describing repo (or a sibling service in this one). Resolved by cloning. |
| [`backing`](#kind-backing) | An image this service provisions for itself — a database, cache, broker. One per declaring service. |
| [`shared-backing`](#kind-shared-backing) | A reference to a `backing` already owned by another service. Binds to that instance instead of starting a second. |
| [`task`](#kind-task) | A container that runs to completion and stays exited — a migration or seed. |

### `kind: service`

A dependency on another self-describing service repo, resolved by
cloning it into the workspace (folder named after the repo URL's last
path segment).

```yaml
# target repo declares one service: omit `services`, it's used automatically
- kind: service
  repo: git@github.com:acme/notifications-service.git
  default_branch: main

# target repo declares several: one block per repo still, name each one you need
- kind: service
  repo: git@github.com:acme/payments-service.git
  default_branch: main
  services: [payments-api, payments-worker]
```

| Field | Description |
|---|---|
| `repo` | Git URL — `git@…`, `https://…`, or `ssh://…`. |
| `services` | Which of the target repo's `services` this depends on — a list, so depending on several from the same repo is still one block (`repo`/`default_branch` stated once), not one block per service. Omit when that repo declares exactly one service (used automatically); required when it declares more than one. Each name gets its own `depends-on` edge; a name that doesn't exist there is a warning, not a hard `fghj validate` failure. |
| `default_branch` | The branch fghj clones this repo at the first time it fetches it — `git clone --branch <it> --single-branch`, defaulting to `main`. Its purpose is that a dependency you've never had on this machine arrives *ready to run*: you're declaring the edge, so you're the one who knows which branch of that repo works against yours. Read once, at clone time. Re-pulling a checkout that already exists only verifies its `origin` and never moves it off the branch you're on, so this is a starting point and never a live pin — see [Branch ownership model](/concepts/branch-ownership-model/). |

### `kind: backing`

A dependency on a backing service — a datastore, broker, or similar —
provisioned directly from an image. Nothing to clone, no `.fghj.yaml` of
its own. The declaring service *owns* this instance; other services can
bind to the same instance via `kind: shared-backing` below.

```yaml
- kind: backing
  name: postgres
  image: postgres:16
  ports: ["5432"]
  environment:
    POSTGRES_PASSWORD: dev
  domain_scope: run
  command: ["mysqld", "--sql_mode=NO_ENGINE_SUBSTITUTION"]
  platform: linux/amd64
  env_file:
    - .env.postgres
  restart: unless-stopped
  healthcheck:
    test: ["CMD", "pg_isready"]
    interval: 5
    retries: 5
  volumes:
    - name: pgdata
      container: /var/lib/postgresql/data
```

| Field | Description |
|---|---|
| `name` | Lowercase label, unique among this service's own backing dependencies. |
| `image` | Docker image reference. |
| `ports` | Either a bare list of container ports to publish (`["5432"]`), or the same `{port: #Port}` map form `service.ports` uses (see [Ports](#ports) above) — e.g. `minio`'s S3 API and web console can each get their own `primary`/`name`/`wildcard`/`host_port` this way, exactly like a service's own ports. |
| `environment` | Same shape as `service.environment`. |
| `domain_scope` | `"run"` (default) or `"stable"` — same semantics as `service.domain_scope`. |
| `command` | Same shape as `service.command` — overrides the image's default `CMD`, e.g. to pass extra startup flags to a stock database image. |
| `platform` | Pins the image's platform (`os[/arch[/variant]]`, e.g. `linux/amd64`) — for a backing image only published for one architecture, so Docker's platform-aware pull/lookup gets the right one. |
| `env_file` | Same shape as `service.env_file`, but resolved differently: since a backing dependency has no checkout of its own, each path resolves against the *declaring* service's checkout root instead — the same rule Compose uses, resolving `env_file` against the compose file's own directory regardless of `build` vs `image`. |
| `restart` / `stop_signal` / `stop_grace_period` / `user` / `working_dir` / `labels` / `cap_add` / `cap_drop` / `privileged` / `extra_hosts` / `healthcheck` / `debug` | Same shape and meaning as the equally-named `service.*` fields above — they come from one shared `#RunOptions` definition, not two parallel ones. A stock database image is the most likely place you'll actually want `stop_grace_period`. |
| `volumes` | Same shape as `service.volumes` — see [Volumes](#volumes). A volume's own `scope` (default `"run"`) governs its lifecycle independently of this backing dependency's `domain_scope`; a named volume here (like `pgdata` above) is what makes the data survive a restart. |

### `kind: shared-backing`

A reference to a `kind: backing` dependency already owned by another
service in the resolved graph — binds to that same running instance
instead of provisioning a second one. The owning service can be in
another repo, or a sibling service declared in this same repo's
`services` map — e.g. a `vite` dev-server service and a `php` service
built from the same repo, where `php` owns a `mysql` backing dependency
that `vite` also needs to reach:

```yaml
# cross-repo: omit `service` if the target repo only declares one
- kind: shared-backing
  repo: git@github.com:acme/payments-service.git
  service: payments-api
  name: postgres

# same-repo: omit `repo` entirely — refers to a sibling service
# declared in this repo's own `services` map
- kind: shared-backing
  service: php
  name: mysql
```

| Field | Description |
|---|---|
| `repo` | The Git URL of the repo that owns the backing dependency. Omit to reference a sibling service in this same repo instead. |
| `service` | The name of the service (in `repo`, or in this repo if `repo` is omitted) that owns the backing dependency. |
| `name` | Must match the owning service's declared backing dependency name exactly. A reference that doesn't resolve is flagged as a warning, not a hard failure — the owning repo might just not be cloned yet. |

### `kind: task`

A container that runs **to completion** and then stays exited — a migration,
a seed, a fixture loader. This is a distinct node kind rather than a flag on a
service, because `exited` means opposite things for the two: drift for a
service, success for a task. See
[Terminating nodes](/concepts/terminating-nodes/) for the full argument, and
[tutorial chapter 3](/tutorial/03-a-migration/) for a worked example.

```yaml
# runs the owning service's own image with a different command
- kind: task
  name: migrate
  command: ["./bin/migrate"]
  after: ["db"]

# runs a stock image instead
- kind: task
  name: seed
  image: postgres:16
  command: ["psql", "-v", "ON_ERROR_STOP=1", "-f", "/fixtures.sql"]
  after: ["db"]
  run: once
  environment:
    PGHOST: ${FGHJ_SERVICE_FQDN:db}
  volumes:
    - host: ./fixtures.sql
      container: /fixtures.sql
      read_only: true
```

| Field | Description |
|---|---|
| `name` | Lowercase label, unique among this service's own dependencies. The node id becomes `{name}.{owning service's id}`, same as a backing dependency. |
| `command` | **Required, and non-empty** — a task *is* its command. Without one it would run the image's default `CMD`, which for an inherited image is the service's own long-running entrypoint: it would never exit, and the run would hang until the task budget expired. |
| `image` | Optional. Omitted, the task runs the **owning service's own built image** — the usual case, since a migration is that service's code with a different command (`rake db:migrate`, `alembic upgrade head`). Give one only for a task that genuinely isn't the owner's code. The owner must declare a `build` if this is omitted, or there's nothing to inherit. |
| `after` | Order this task after other dependencies of the *same* owning service, named as they name themselves: a sibling backing dependency's or task's `name`, or a sibling `kind: service` dependency's service name. Defaults to `[]`. Scoped to siblings deliberately — ordering against an arbitrary node elsewhere would be an edge between two repos that never agreed to one. An `after` naming no sibling is a blocking warning, not a dangling edge. |
| `run` | `"on_start"` (the default) re-runs on every start and every top-up, which is what a migration wants — its command is expected to be idempotent. `"once"` runs it at most once per run, for the expensive or destructive case. See the note below on what `once` costs. |
| `environment` / `volumes` | Same shape as the equally-named `service.*` fields, including `${FGHJ_SERVICE_FQDN}` templating. A bind mount's relative `host` path resolves against the **owning service's** checkout root, since a task has no checkout of its own. |
| `restart` | Forced to `"no"`, not merely defaulted — a restart policy on a container whose whole purpose is to exit would restart it forever. |
| `healthcheck` | **Not allowed.** Declaring one is a schema error rather than a silently ignored field: an exited container can never report Docker-`healthy`, which is precisely the hole this kind fills. |

A task has no ports, no domain, and no restart policy. It's never routed to
and never published.

**Ordering is load-bearing, not decoration.** A task isn't considered
*started* until it has **finished**: fghj waits for the container to exit, and
a task that exits non-zero — or never exits — fails the node. Both `start` and
`ensure_running` stop on a node error, so a failed migration blocks everything
downstream of it rather than letting the service come up against an
unmigrated database. With the first example above, the start order is
`db → migrate → api`.

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

```yaml
flows:
  checkout:
    description: End-to-end checkout journey
    service: cart-service
    dependencies:
      - kind: service
        repo: git@github.com:acme/payments-service.git
        default_branch: main
```

Any repo can declare zero or more flows — there's no distinguished "root"
repo; see [Flat workspace model](/concepts/flat-workspace-model/). Each
flow is a named user journey: a description plus an additional list of
dependencies (any of the four kinds above) pulled in only when that flow is
selected, on top of the service's own baseline `dependencies`.

`service` says which of this repo's `services` the flow is rooted at.
Omit it when the repo declares exactly one service (it's used
automatically); required when it declares more than one, since there's
no other way to tell which service's dependencies the flow is actually
describing. An ambiguous or missing reference is a warning, not a hard
`fghj validate` failure.

`dependencies` is required, but may be empty. `dependencies: []` is a legal
flow: it names a journey that needs nothing beyond the root service's own
baseline graph, and selecting it still does something — a flow selection is
a highlight, so it dims everything outside that subgraph (see
[Fog-of-war visibility](/concepts/fog-of-war-visibility/)). Omitting the
key entirely is a different matter and is refused.
