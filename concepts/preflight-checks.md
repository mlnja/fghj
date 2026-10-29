# Preflight checks: `fghj doctor` and the Doctor tab

> **Status:** implemented. `src/doctor.rs` (`Verdict`, `Check`, `Report`,
> `daemon_report`, `client_checks`); served by `src/web/api/control.rs`
> (`get_daemon_doctor`) at `GET /daemon/doctor`; rendered by `src/main.rs`
> (`Commands::Doctor`, `doctor`, `print_check`, and `apply_fixes` for
> `--fix`) and
> `ui/src/lib/TelemetryDrawer.svelte` (the `'doctor'` tab).

## The hole this fills

fghj takes more from the host than a dev tool normally does. A Docker daemon
new enough to offer BuildKit ([[build-inputs]]), ports 80 and 443 bound on a
loopback address of fghjd's own, which on macOS does not exist until
something aliases it ([[local-ca-and-tls-proxy]]), OS resolver config
pointing at a DNS port (`/etc/resolver` files on macOS, the `fghj0` link on
Linux; [[split-dns]]), and a root CA trusted by the system (the System
keychain on macOS; the system store, and each workspace owner's browsers, on
Linux). Six dependencies, each of which can go missing on its own — a Docker
Desktop upgrade, a stray `sudo ifconfig lo0 -alias`, an OS update that resets
`/etc/resolver` or restarts systemd-resolved, a keychain reset, a new browser
profile, another process grabbing 443.

Every one of those failures produces the same symptom. The browser says
`ERR_CONNECTION_REFUSED` and nothing else. Which of the six broke is not
visible from there, not visible from the UI's graph, and not visible from
`fghjd`'s log unless the breakage happened while it was watching.

So: one command that checks all six and names the one that's wrong.

## Read the host, never the intent

Every check reads the real thing, not fghj's record of what it wanted. The
daemon already knows what it *intended* — `DaemonControl::active`, the
desired `/etc/hosts` block, the computed route set. That knowledge is exactly
what's useless here, because the failure mode being diagnosed is precisely
"what fghjd believes and what the machine does have diverged."

This is the same principle `/daemon/net-status` already follows for its three
panels ([[control-api-and-cli]]), applied to the rest of the host surface and
turned into verdicts rather than raw listings.

## The DNS check is the one that earns its keep

The OS resolver config (`/etc/resolver/fghj.internal` on macOS, the `fghj0`
link on Linux) records the port fghjd's DNS server is listening on. That port is **ephemeral** — assigned by the OS on each
`DaemonControl::activate` and stored nowhere else, not in `DaemonControl`, not
in `persistence::DaemonState`. The resolver config is the only record of it.

Which means config written by a *previous* fghjd process is perfectly
well-formed and points at a port nobody is listening on. It passes any
inspection of its contents. `DnsRouting::status` returns it happily. And every `*.fghj.internal` name on the machine silently fails to
resolve.

The only way to catch that is to resolve a name the way a browser would.
`dns_resolution_check` calls `getaddrinfo` (via `ToSocketAddrs`) on
`doctor.fghj.internal` and asserts the answer is `proxy::PROXY_IP`. Any name
in the zone works, because the server answers `dns::ANSWER` for the whole
zone regardless of whether a workspace has claimed that name — so the probe
needs no running workspace and no coordination with the registry.

It also distinguishes the two interesting wrong answers. No answer at all
means the resolver path is broken; an answer that isn't `PROXY_IP` means
something *else* on this machine is authoritative for the zone, which is a
different problem with a different fix.

## Four verdicts, and why `Warn` and `Pending` exist

`Pass`/`Pending`/`Warn`/`Fail`, and only `Fail` sets the exit code
(`Report::has_failures`).

`Pending` (`·`) is "not set up yet, and nothing to do": the first
`fghj wire` is what tells fghjd which Docker to use and gets the CA trusted,
so until anything is wired the Docker, BuildKit and CA checks are pending
rather than failing. Once something is wired the same conditions fail,
because then something really is wrong. Without it a fresh install would
open on a wall of red for a step the user simply hasn't reached.

`Warn` is not "a mild failure." It's for two specific situations where red
would be a lie:

- **Deliberate state.** `fghj daemon stop` leaves fghjd idle on purpose.
  Every host check below it then fails *correctly*. Reporting that as a wall
  of red would mean the doctor calls a machine broken for doing exactly what
  it was told, so `daemon_check` warns, and its hint says so.
- **A feature you may not use.** `cue` is only needed by `fghj validate`;
  the daemon resolves and runs workspaces without it. Not having it isn't a
  broken install.

The split matters because the exit code is meant to be usable: `fghj doctor
&& …` should gate on "is fghj broken", not on "is anything less than
perfect."

## Checks never fix; `--fix` does, on request

No check mutates anything. The hints name an existing command instead —
`fghj daemon start` or `restart` for the alias, the ports and the resolver
config, which `activate` re-establishes every time it runs, and
`fghj doctor --fix` for the rest.

A self-healing doctor is a doctor that can't tell you what was wrong: the
second run is always green, so the thing that keeps breaking stays invisible.
So plain `fghj doctor` only reports — its output also goes into bug reports,
which it mustn't change — and `--fix` is a separate, explicit request. It
runs the checks, applies the fixes it knows for the ones that didn't pass
(point fghjd at the user's Docker, activate it, trust the CA from the user's
terminal on macOS, `POST /daemon/trust` for the browsers on Linux), says what
it did, and only then reports again.

The one borderline case is `ca_trust_check`, which calls
`TrustStore::is_trusted`. On macOS that's a `security verify-cert`, a trust
*evaluation* that never raises the Authorization Services prompt
`add-trusted-cert` does; on Linux it reads the system bundle back, and
`browser_trust_check` lists each NSS database with `certutil -L`. Read-only
in the sense that matters: none of them change a store or block on a
human.

## Where each check runs, and why most of them are daemon-side

`client_checks` runs in the CLI process and has exactly two checks: the
control socket connects, and `cue` is on `PATH`. Everything else is in
`daemon_report`, because everything else needs something only `fghjd` has:

- the Docker client **the workspaces actually run through**
  (`WorkspaceRegistry::docker`) — a fresh client built in the CLI would test
  whatever the user's own context resolves right now, which is not
  necessarily the daemon the running containers are on;
- the workspace owners, for the per-owner browser trust check on Linux;
- the activation state;
- each workspace's environment, for the per-workspace `sidecar:<workspace>`
  check. That one is the exception to "read the host": whether the sidecar is
  running is read from Docker, but *why* it won't come back is only known to
  the reconciler, which keeps its last restore error in
  `DaemonControl::sidecar_failures` until a restore succeeds or the sidecar
  is seen running;
- root. The CA lives under `/var/lib/fghjd`, which the invoking user cannot
  read ([[persistence-and-workspace-store]]).

`fghj doctor` prints the client checks *first*, before asking the daemon
anything. When the socket is the thing that's broken, the first line of
output is already the whole answer and there is nothing to ask.

## BuildKit has no direct signal

Nothing in `/version` or `/info` reports "BuildKit" as a server capability.
What the gRPC driver actually needs is the Engine's `/session` endpoint,
which arrived in API 1.39 (Engine 18.09) — so that's the threshold
(`BUILDKIT_MIN_API`), compared as a numeric `(major, minor)` pair rather
than as a string, because `"1.9"` lexically outranks `"1.39"`.

This check exists because [[build-inputs]] removed the classic builder
entirely. With one builder and no fallback, "your Docker is too old" went
from a per-build surprise to a flat precondition — which is the kind of thing
a preflight check is for.

## The UI tab is fetched, not polled

The other two telemetry tabs poll every 1.5–2s. The Doctor tab fetches once
when it opens and then only on the explicit "re-run" button
([[ui-architecture]]).

Two reasons, and both are real. The checks shell out to the OS — `ifconfig`,
`getaddrinfo`, `security verify-cert`, three TCP connects — so polling would
spend genuine work several times a second. And nothing the tab measures
changes on its own: an alias does not come back, trust does not reinstate
itself. The useful interaction is read it, go fix it, press re-run — which is
a button, not a timer.

`trust::is_trusted` was switched from `status()` to `output()` as part of
this, purely to swallow the "certificate verification successful" line
`security` prints on every call. At startup that was one stray line; on
demand from a UI it would have scattered through `fghjd`'s log.
