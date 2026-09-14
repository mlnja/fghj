---
title: In-network TLS proxy sidecar
description: Why fghj runs a dedicated proxy container per run, the approaches that were rejected, and the platform pitfalls that shaped the implementation.
---

This page is a decision record for the in-network sidecar proxy — the "why"
behind the design, not a usage guide. For how a container actually reaches
it (nothing to opt into — automatic for every node), see [Reaching the TLS
proxy from inside a
container](/reference/fghj-yaml/#reaching-the-tls-proxy-from-inside-a-container).
For the TLS/CA mechanics it reuses, see [Local CA & TLS
proxy](/concepts/local-ca-and-tls-proxy/#reaching-the-proxy-from-inside-a-runs-own-network),
and for the DNS split that makes it automatic, see [Split
DNS](/concepts/split-dns/).

## The problem

`fghjd`'s reverse proxy binds to `127.0.0.1` on the host. That's fine for a
browser or a host process, but breaks a real workflow: a service that hands
out URLs pointing at a sibling container (a presigned S3 URL against a
`minio` backing dependency is the motivating case) needs the *same*
hostname to work both for its own internal calls (from inside the run's
docker network) and for whoever consumes the URL it handed out (from
outside it, e.g. a browser). Docker's own embedded per-network DNS already
resolves a sibling's domain straight to that container's IP for anyone
asking from inside the network — bypassing TLS and the proxy entirely,
since only the host-side proxy has a listener behind that name.

## Two rejected approaches

Both were tried and worked in some sense, but neither was acceptable:

- **`extra_hosts: [...:host-gateway]`** — pins the hostname to the host's
  gateway IP, reaching the host-side proxy from inside the container. It
  worked on OrbStack. Rejected anyway: it only works *because* of
  OrbStack/Docker Desktop's gateway forwarding, isn't portable to plain
  Linux Docker Engine, and doesn't compose with wanting a genuinely separate
  docker network per run for isolation.
- **Binding a host process directly to a docker bridge network's gateway
  IP** — fails outright on OrbStack (`Can't assign requested address`) even
  though the subnet is otherwise routable from macOS (ping works). There's
  no real bindable interface there to use.

The only architecturally sound fix was a real **container** on each run's
own network, speaking the same TLS/SNI-dispatch protocol the host proxy
already does.

## Design decisions

**One sidecar per run, not shared across runs or workspaces.** Preserves
the same per-run network isolation every other part of a run already has —
a shared sidecar would mean one run's container list is reachable from
another run's network.

**A separate `fghj-sidecar` binary (`src/bin/fghj-sidecar.rs`), not a mode
flag on `fghjd`.** This container gets the CA's private key bind-mounted
in, so it deserves its own minimal, easy-to-audit entrypoint rather than a
branch inside a binary that also unconditionally requires root and runs the
full control API.

**Widened `RouteResolver` (`src/proxy.rs`) to resolve to a full `Backend {
host, port }`, not just a port.** The host-side proxy always relayed to
`127.0.0.1` and only needed a port; the sidecar relays to sibling
containers by their own network address. One trait, one `serve_https`
/`serve_http_redirect` implementation, two different `resolve()` backings —
the host-side one still hardcodes `127.0.0.1`, the sidecar's reads a route
file. `ca.rs` needed no change: it only ever checked `.is_some()` on the
result.

**Synced via a bind-mounted, polled JSON file — no network call between
`fghjd` and the sidecar.** `fghjd` writes the route table on every
container/route change; the sidecar polls the file's mtime once a second
and reloads on change. Polling, not inotify: bind-mount filesystem-event
propagation across the OrbStack/Docker-Desktop virtualization boundary is
unreliable — exactly the kind of platform-specific gap this whole feature
exists to stop depending on. A 1s poll of one small file has no
missed-event failure mode and costs nothing.

**Route entries name the owning container's own domain/alias as the
connect target, not a raw IP.** Every routable domain is already a real
Docker network alias on the container it targets, so Docker's own embedded
DNS resolves it correctly for the sidecar (or any other container on that
network) with no separate IP bookkeeping.

**The sidecar's Docker image is built by embedding the whole crate into
`fghjd` at compile time** (`src/sidecar_image.rs`, `include_dir!`, same
trick `server.rs` uses for the UI) and materializing it to
`/var/lib/fghjd/sidecar-build/` on first use. `fghjd` ships as a prebuilt
binary with no cargo workspace on the target machine, so there's no
existing source tree to hand to `docker build` — this makes the sidecar
buildable from nothing but the `fghjd` binary itself. Tagged
`fghj-sidecar:<CARGO_PKG_VERSION>` and only rebuilt if that tag doesn't
already exist, so a normal `fghjd` restart doesn't rebuild every time.
Known limitation: iterating on the sidecar's own source during development
isn't picked up without bumping the crate version or removing the cached
image tag by hand.

**The sidecar is also this run's in-network DNS authority — no per-consumer
opt-in.** An earlier design routed a hostname to the sidecar with a literal
`extra_hosts: ["host:fghj-proxy"]` sentinel, rewritten to the run's actual
sidecar IP at container-start time. That was retired (see the limitation
below) in favor of making the sidecar answer DNS for the whole network:
every node's container points `--dns` at the sidecar first, and the
sidecar answers any `*.fghj.internal` name (or active alias) with its own
IP, forwarding everything else — including every `*.fghj.raw.internal`
query — verbatim to Docker's embedded resolver. Same route table it
already polls for TLS/SNI dispatch, reused as the DNS answer source, so
there's no second data plane to keep in sync. Full mechanics in [Split
DNS](/concepts/split-dns/).

## Platform pitfalls found while building this

None of these are sidecar-specific in principle, but the sidecar is what
first exercised these code paths, so they surfaced here.

**macOS's `/var` is a symlink to `/private/var`, and OrbStack's bind-mount
source resolution doesn't follow it.** A literal `/var/lib/fghjd/...`
source path resolved inside the Docker VM's own internal filesystem instead
of the real macOS host path — silently producing an empty mount instead of
an error. Fixed by `std::fs::canonicalize()`-ing every bind-mount source
path before formatting it into a bind string. This is a no-op (and
therefore safe) on native Linux Docker Engine, where no such symlink
exists, and should hold on Docker Desktop for Mac too, which shares the
same VM-plus-`/private` architecture as OrbStack — unlike the
`host-gateway` approach this feature replaces, this fix isn't tied to one
specific container runtime's behavior.

**Docker Desktop/OrbStack's macOS file-sharing bridge runs as the logged-in
user, not root — even for a container that itself runs as root.** The real
CA key at `daemon::ca_dir()` is deliberately `0600` and root-owned. Mounting
it into the sidecar failed with a permission error despite the sidecar
process reporting `uid=0`, because the host-side bridge process that
actually opens the file for sharing runs as the real macOS user and enforces
its own permission check *before* the request ever reaches the container's
UID namespace — verified directly: even outside any container, `cat` on
that file failed the same way as the logged-in user. Fixed by
`refresh_sidecar_ca_copy()` in `runs.rs`, which keeps a separate `0644`
world-readable copy of the CA cert+key at `/var/lib/fghjd/sidecar-ca/`,
refreshed on every sidecar (re)creation, and mounts *that* into the
sidecar instead of the real CA directory. The real, `0600` CA key is never
touched or exposed; only a copy is made more permissive, and mounting the
CA into a container at all was already this feature's accepted tradeoff —
this only extends readability to whoever can already run `sudo fghjd` on
the machine.

**A read-only bind mount can't have another mount created inside it.**
Mounting the routes directory at `/etc/fghj-sidecar` and then trying to
mount the CA directory at `/etc/fghj-sidecar/ca` failed — Docker/runc can't
create a new mountpoint inside an already-mounted read-only filesystem.
Fixed by using sibling paths instead of nesting one under the other:
`/etc/fghj-sidecar/routes` and `/etc/fghj-sidecar/ca`.

## A bookkeeping bug this surfaced (not sidecar-specific)

Live-testing this feature against a real multi-container workspace exposed
a pre-existing bug in `RunRegistry::new()` (`runs.rs`), which reloads
persisted run state from the database on every `fghjd` startup and checks
each tracked container is still alive in Docker. The reconciliation was
all-or-nothing: if even *one* container in a run failed its liveness check,
the code discarded the tracking for the *entire* run, not just that one
container — silently orphaning every other still-running container from
`fghjd`'s bookkeeping (and, downstream, from the sidecar's route table,
which is derived from exactly that list). Fixed to reconcile per-container
instead: drop only the containers that are actually gone, keep the run and
whatever's still alive. `ensure_running` also now re-describes any node
that Docker reports as alive but that's missing from the tracked container
list, rather than trusting "alive" alone to mean "already fully known" —
which makes the route table self-healing against this class of bug even if
some other path manages to hit it again.

## A limitation this surfaced: one hostname can't safely carry two kinds of traffic

The `fghj-proxy` sentinel rewrote what a hostname resolved to for *every*
connection a container made to it, on any port — not just the ones going
through the proxy. That broke a hostname also used directly on a raw port
elsewhere in the same container's config: live-testing against
`aikifactory` (whose `AIKIFACTORY_S3_ENDPOINT` talks to `minio` directly
over plain HTTP on port `9000`, using the exact same domain also opted into
the sentinel for HTTPS testing) broke `aikifactory`'s own uploads outright
— the sentinel pointed that hostname at the sidecar, whose only listeners
are `80`/`443`, so every port-`9000` call started hitting
`connection refused`.

The root cause isn't fixable per-hostname: raw TCP carries no in-band
signal (no SNI, no Host header) for "which backend do you mean" before
bytes start flowing, so one shared IP can never safely multiplex raw-port
traffic across backends that happen to share a port number. Only HTTP(S)
traffic, where SNI/Host is readable before routing, can ever be served off
one shared IP. The actual fix was splitting the zone in two —
`fghj.internal` (HTTP(S)-canonical, proxied, safe to share one IP) and
`fghj.raw.internal` (raw/direct, resolved straight to the real container
IP via Docker's native per-network DNS, exactly like a hostname isn't
shared today) — rather than trying to patch the sentinel further. Docker
network aliases alone couldn't implement the http side of the split
(they're exact-match only, and can't express `wildcard_hosts` or be added
to an already-connected container), which is what pushed the sidecar into also
being a DNS forwarder (see the design decision above and [Split
DNS](/concepts/split-dns/)) instead. This is also why the sentinel itself
was fully retired: with the sidecar universally reachable via DNS for the
right zone, there's no longer a hostname that needs opting in at all.

## Verified end-to-end

`minio.<...>.fghj.internal` was confirmed reachable identically via both
paths against a real workspace, prior to the zone split above: same
hostname, no port suffix on either side, same fghj-CA-issued certificate
(`openssl x509 -noout -subject -issuer` matched byte-for-byte), and the
same live `minio` backend behind both — the host-side proxy from outside
the network, the sidecar (via the since-retired `fghj-proxy` sentinel)
from inside it. Re-verification against the same workspace, covering both
zones and the now-automatic reachability, is tracked separately.
