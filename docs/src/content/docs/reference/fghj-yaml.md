---
title: fghj.yaml
description: Full reference for the fghj.yaml file every repo declares — services, ports, dependencies, and flows.
---

Every repo that participates in `fghj` carries its own `fghj.yaml` at its
root. There's no shared/root config — each file is self-contained and
validated independently against fghj's CUE schema; see
[fghj validate](/cli/validate/). The full grammar lives in `schema/component.cue`
and `schema/dependency.cue` in the fghj repo.

## Top-level shape

```yaml
version: "1.0"
service:
  # ... see Service below
flows:
  <flow-name>:
    # ... see Flows below
```

## `service`

```yaml
service:
  name: cart-service
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
  dependencies:
    - kind: service
      repo: git@github.com:acme/auth-service.git
      default_branch: main
```

| Field | Type | Description |
|---|---|---|
| `name` | string | Lowercase, `[a-z0-9][a-z0-9-]*`. Human-readable label — not the node's internal id. See [Node identity & domains](/concepts/node-identity-and-domains/) for why those differ. |
| `build.context` | string | Docker build context. Defaults to `.`. |
| `build.dockerfile` | string | Dockerfile path, relative to `context`. Defaults to `Dockerfile`. |
| `build.args` | map of string→string | Build-time `--build-arg` values. |
| `ports` | map of container-port→`#Port` | Declared container ports. The map key is the literal container port number (e.g. `"8080"`), published to Docker as-is — not a semantic label. See [Ports](#ports) below. |
| `domain_scope` | `"run"` \| `"stable"` | Whether this service's derived domain includes the run id. Defaults to `"run"`. See [Node identity & domains](/concepts/node-identity-and-domains/#domain-derivation-one-formula-no-exceptions). |
| `environment` | map or list | Either `{KEY: value}` or a list of `"KEY=value"` strings — mirrors Docker Compose's own `environment` shape. |
| `volumes` | list of `#Volume` | Bind mounts and named volumes. See [Volumes](#volumes) below. |
| `additional_hosts` | list of `#AdditionalHost` | Extra literal hostname aliases this service also answers on, alongside its derived domain. See [Additional hosts](#additional-hosts) below. |
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

A port with neither `primary` nor `name` is still published to an
ephemeral localhost port, just with no `*.fghj.internal` name.

## Volumes

Each entry is **either** a bind mount **or** a named volume, distinguished
by which key you set — `host` for a bind mount, `name` for a named volume.
Setting both, or neither, fails `fghj validate`.

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
*derived* from it (folding in the workspace and, depending on `scope`, the
run), never the literal string you write. That derivation is also the
entire sharing mechanism: **any other node** — a different service, a
different backing dependency, related or not — that declares the same
`name` **and** the same `scope` resolves to that same derived name and
therefore shares the same underlying storage, with no ownership
relationship required:

```yaml
# service A's fghj.yaml
volumes:
  - name: shared-cache
    container: /app/.cache

# service B's fghj.yaml — same name, same scope, same volume
volumes:
  - name: shared-cache
    container: /var/cache/app
```

| Field | Type | Description |
|---|---|---|
| `name` | string | A bare label. Two nodes with the same `name` + `scope` share one Docker volume. |
| `scope` | `"run"` \| `"stable"` | Same semantics as `domain_scope`: `"run"` (the default) gives each run (including preview/named runs) its own fresh empty volume; `"stable"` gives the volume one fixed identity that persists across every run. |
| `container` | string | Mount path inside the container. |
| `read_only` | bool | Mounts read-only. Defaults to `false`. |

Named volumes are never deleted by `fghj` — stopping a run tears down its
containers and network but leaves the volume's data in place, which is
what makes it "persistent" in the first place. A `"run"`-scoped preview
run that you stop and never restart leaves its volume behind; there's no
`docker compose down -v` equivalent yet to clean those up.

## Additional hosts

A service is normally only reachable at its derived `*.fghj.internal`
domain (see [Node identity & domains](/concepts/node-identity-and-domains/)).
`additional_hosts` lets it also answer on one or more extra, literal
hostnames — useful when something outside fghj already has a hostname on
file, like a third-party OAuth callback pointing at `aikido.local`, and
reconfiguring that third party just to fit fghj's own domain isn't
practical.

```yaml
service:
  name: aikido-core
  ports:
    "3000":
      primary: true
  additional_hosts:
    - aikido.local
    - app.local.aikido.io
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

## Dependencies

Three kinds, distinguished by `kind`:

### `kind: service`

A dependency on another self-describing service repo, resolved by
cloning it into the workspace (folder named after the repo URL's last
path segment).

```yaml
- kind: service
  repo: git@github.com:acme/payments-service.git
  default_branch: main
```

| Field | Description |
|---|---|
| `repo` | Git URL — `git@…`, `https://…`, or `ssh://…`. |
| `default_branch` | The branch cloned by default. This is only ever a *default* — never a live pin; see [Branch ownership model](/concepts/branch-ownership-model/). |

### `kind: backing`

A dependency on a backing service — a datastore, broker, or similar —
provisioned directly from an image. Nothing to clone, no `fghj.yaml` of
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
  volumes:
    - name: pgdata
      container: /var/lib/postgresql/data
```

| Field | Description |
|---|---|
| `name` | Lowercase label, unique among this service's own backing dependencies. |
| `image` | Docker image reference. |
| `ports` | List of container ports to publish. |
| `environment` | Same shape as `service.environment`. |
| `domain_scope` | `"run"` (default) or `"stable"` — same semantics as `service.domain_scope`. |
| `volumes` | Same shape as `service.volumes` — see [Volumes](#volumes). A volume's own `scope` (default `"run"`) governs its lifecycle independently of this backing dependency's `domain_scope`; a named volume here (like `pgdata` above) is what makes the data survive a restart. |

### `kind: shared-backing`

A reference to a `kind: backing` dependency already owned by another
service in the resolved graph — binds to that same running instance
instead of provisioning a second one.

```yaml
- kind: shared-backing
  repo: git@github.com:acme/payments-service.git
  name: postgres
```

| Field | Description |
|---|---|
| `repo` | The Git URL of the service that owns the backing dependency — not that service's declared `name`, since names aren't unique across peer repos. |
| `name` | Must match the owning service's declared backing dependency name exactly. A reference that doesn't resolve is flagged as a warning, not a hard failure — the owning repo might just not be cloned yet. |

## `flows`

```yaml
flows:
  checkout:
    description: End-to-end checkout journey
    dependencies:
      - kind: service
        repo: git@github.com:acme/payments-service.git
        default_branch: main
```

Any repo can declare zero or more flows — there's no distinguished "root"
repo; see [Flat workspace model](/concepts/flat-workspace-model/). Each
flow is a named user journey: a description plus an additional list of
dependencies (same three kinds as above) pulled in only when that flow is
selected, on top of the service's own baseline `dependencies`.

A flow's `dependencies` list must be non-empty — a flow with zero extra
dependencies isn't meaningfully different from the service's baseline
graph.
