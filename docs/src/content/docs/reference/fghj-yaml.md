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
    http:
      primary: true
    admin:
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
| `ports` | map of string→`#Port` | Declared container ports. See [Ports](#ports) below. |
| `domain_scope` | `"run"` \| `"stable"` | Whether this service's derived domain includes the run id. Defaults to `"run"`. See [Node identity & domains](/concepts/node-identity-and-domains/#domain-derivation-one-formula-no-exceptions). |
| `environment` | map or list | Either `{KEY: value}` or a list of `"KEY=value"` strings — mirrors Docker Compose's own `environment` shape. |
| `dependencies` | list of `#Dependency` | This service's baseline dependencies — always pulled in regardless of which flow is selected. See [Dependencies](#dependencies) below. |

## Ports

```yaml
ports:
  http:
    primary: true
  admin:
    name: admin
  raw:
    host_port: 9000
```

| Field | Type | Description |
|---|---|---|
| `primary` | bool | At most one port per service should set this. Puts the port at the service's own domain (`cart.myworkspace.fghj.internal`). Defaults to `false`. |
| `name` | string, optional | Gives the port an *additional* nested domain: `{name}.{service's domain}`. Can be combined with `primary`. |
| `host_port` | 1–65535, optional | Pin the host-side published port instead of letting Docker assign a random ephemeral one — for protocols whose clients hardcode a port and can't go through name-based routing at all. Only one run can hold this exact host port at a time. |

A port with neither `primary` nor `name` is still published to an
ephemeral localhost port, just with no `*.fghj.internal` name.

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
```

| Field | Description |
|---|---|
| `name` | Lowercase label, unique among this service's own backing dependencies. |
| `image` | Docker image reference. |
| `ports` | List of container ports to publish. |
| `environment` | Same shape as `service.environment`. |
| `domain_scope` | `"run"` (default) or `"stable"` — same semantics as `service.domain_scope`. |

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
