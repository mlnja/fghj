---
title: fghj doctor
description: Check that everything fghj needs from this machine is actually there, and name the one thing that isn't.
---

```bash
fghj doctor
```

fghj takes more from your machine than most dev tools do: a Docker daemon
new enough for BuildKit, ports 80 and 443 bound on a loopback alias that
doesn't exist until `fghjd` creates it, OS DNS routing for
`*.fghj.internal`, and a root CA trusted by your system keychain.

Each of those can go missing on its own — a Docker upgrade, an OS update
that resets `/etc/resolver`, a keychain reset, another process grabbing 443 —
and when one does, every one of them produces the same symptom: the browser
says the connection was refused, and nothing says why.

`fghj doctor` checks them all and tells you which one it is.

```
$ fghj doctor
✓ fghjd control socket: connected to /var/run/fghjd.sock
✓ cue CLI on PATH: cue version v0.17.1
✓ Docker Engine reachable: server 29.4.0, API 1.54
✓ BuildKit available: API 1.54 supports the BuildKit session endpoint
✓ fghjd is active: serving DNS, 80 and 443
✓ proxy loopback alias: 127.222.0.1 is up on lo0
✓ proxy listening on 443: 127.222.0.1:443 accepts connections
✓ proxy listening on 80: 127.222.0.1:80 accepts connections
✓ /etc/resolver entries: fghj.internal -> 127.0.0.1:49969, fghj.raw.internal -> 127.0.0.1:49969
✓ fghj.internal resolves through the OS: doctor.fghj.internal -> 127.222.0.1
✓ root CA trusted by the system: /private/var/lib/fghjd/ca/ca-cert.pem verifies against the System keychain

all clear
```

A failing check prints what it found and what to do about it:

```
✗ proxy listening on 443: 127.222.0.1:443 is not accepting connections: Connection refused
    → run `fghj daemon start`; if it reports the port is in use, something else
      on this machine has 80/443 (another dev proxy, nginx, Docker Desktop's own
      port binding)
```

## Reading the marks

| Mark | Meaning |
|---|---|
| `✓` | Checked, and it's the way it should be. |
| `!` | Not how it should be, but nothing is broken *yet* — it affects one feature, or fghj will fix it on the next start. |
| `✗` | Something fghj needs is missing or wrong, and it will visibly misbehave until it's fixed. |

Only a `✗` sets a non-zero exit code, so `fghj doctor && …` gates on "is
fghj broken", not on "is anything less than perfect". The two things that
warn rather than fail are worth knowing:

- **`fghjd is active`** warns when the daemon is idle. That's a state you can
  ask for with [`fghj daemon stop`](/cli/daemon/), and every host check below
  it then fails correctly — so being idle is reported as a warning with the
  rest of the failures following from it, not as a broken install.
- **`cue CLI on PATH`** warns when `cue` is missing. Only
  [`fghj validate`](/cli/validate/) needs it; `fghjd` resolves and runs
  workspaces without it.

## The DNS check is the interesting one

`fghjd`'s DNS server listens on a port the OS assigns fresh every time it
starts, and `/etc/resolver/fghj.internal` is the only place that port is
recorded. So a resolver file left behind by a *previous* `fghjd` process
looks completely healthy — the file is there, the syntax is right, the zone
is correct — and points at a port nothing is listening on. Every
`*.fghj.internal` name then fails to resolve, with no visible cause.

That's why the `fghj.internal resolves through the OS` check doesn't inspect
the file. It asks your OS to resolve a name in the zone, exactly as a browser
would, and checks that the answer is fghj's proxy address. `fghj daemon
restart` is the fix, and the hint says so.

## It only reports; it never repairs

No check changes anything on your machine. That's deliberate: a doctor that
fixes things as it finds them is always green on the second run, which makes
a problem that keeps coming back invisible.

Almost every hint points at [`fghj daemon start`](/cli/daemon/) or `restart`,
because that's already the repair path — activating re-adds the loopback
alias, rebinds 80/443, rewrites the resolver files and reinstalls CA trust,
every time it runs.

## The same checks in the UI

The web UI's telemetry drawer (the **⚡** button in the header) has a
**Doctor** tab showing the same report, with a re-run button. It's fetched
when you open the tab rather than polled — nothing it measures changes on its
own, so the useful loop is read it, go fix the thing, press re-run.

The UI's version omits the two client-side checks (`fghjd` is obviously
reachable if the page loaded, and `cue` is a property of *your* shell, not the
daemon's).
