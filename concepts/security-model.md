# The security model: what cloning a repo grants its author

## Why this file exists

Every capability below is individually reasoned in its own concept file, and
each of those arguments is sound on its own terms. What none of them state is
the **composite**, which is the only form in which a user can actually
evaluate the risk:

> Adding a repo to an fghj workspace grants that repo's author the ability to
> run arbitrary code as you, with arbitrary access to your filesystem, on a
> machine where a root daemon is already running and a CA you trust is
> already installed.

That is a large grant. It is also, essentially, the grant you already make by
running `docker compose up` in a cloned repo, or `npm install`, or opening
the folder in an editor with plugins. fghj does not meaningfully widen it.
But "no wider than the neighbours" is a claim worth checking rather than
assuming, so this file enumerates the surface.

## The trust boundary

There is exactly one, and it is **the decision to wire a workspace and clone
a repo into it**. Everything inside that boundary is trusted; nothing is
sandboxed after it.

This is a deliberate choice, not an oversight, and it follows from what the
tool is for. fghj exists to run *your company's services* on *your* laptop —
fifteen private sibling repos written by colleagues. A sandbox between those
repos and your machine would have to be leak-proof to be worth anything, and
a leak-proof sandbox around "build this Dockerfile and run it with these bind
mounts" is a research project, not a feature. Pretending to have one would be
worse than not having one, because users calibrate their caution to what they
think the tool protects.

So: **fghj protects you from accidents, not from the authors of the repos you
clone.**

## What a cloned repo can do

| Capability | Mechanism | Where it's reasoned |
|---|---|---|
| Run arbitrary code at build time | `#Build` — any `Dockerfile`, any `RUN` | [[build-inputs]] |
| Run arbitrary code at run time | any `image`, any `command` | [[docker-and-downloads]] |
| Read and write any path on your disk | `#Volume.host` — bind mounts are **not** sandboxed; an absolute or `..`-escaping path passes straight through to Docker, exactly as in Compose | [[docker-and-downloads]] |
| Request near-host-equivalent container privileges | `privileged: true`, `cap_add` | `schema/dependency.cue` |
| Claim a hostname on your machine | `additional_hosts` → `/etc/hosts`, `wildcard_hosts` → `/etc/resolver` | [[host-aliases]] |
| Reach your ssh-agent during a build | `build.ssh: true` | [[build-inputs]] |

The last row is the one with a real mitigation, and it is worth stating why.
The repo can *request* agent forwarding, but it cannot say **whose** agent or
where the socket is: the path comes from `WorkspaceOwner`, captured by the
unprivileged CLI at `fghj wire` time, never from the repo's config. So the
config surface has a boolean and nothing else. The same applies to build
secrets: a repo names a file *relative to its own checkout*, so it can only
ask for credentials you already put inside the repo you cloned.

That is the pattern worth generalizing: **the config may express intent; the
credential path always comes from the daemon's own record of who you are.**

## What fghj itself holds

The daemon's own privileges are a separate question from what a repo can ask
for.

**`fghjd` runs as root.** It has to: binding ports 80 and 443, writing
`/etc/hosts` and `/etc/resolver`, installing a CA into the System keychain,
and loading `pf` rules all require it. There is no design in which the proxy
listens on 443 unprivileged.

**It does not do everything as root.** `WorkspaceOwner` is a snapshot of the
real user's uid/gid/home/`SSH_AUTH_SOCK`, captured by the unprivileged CLI
and persisted; `apply_to_command` re-applies it to anything that touches the
user's own world — notably `git clone`, which must produce files the user
owns and must use the user's ssh credentials, not root's. Root runs the
daemon; you run the clones. See [[persistence-and-workspace-store]].

**The CA is in the system trust store.** A certificate `fghjd` mints is
trusted by every browser on the machine, which is the entire point and also
the single largest thing fghj holds. Two constraints bound it: the private
key is generated locally and never leaves the machine, and `cert_eligible`
restricts what can ever be signed to fghj's own zone plus *routed* names
under IANA reserved TLDs (`.local`, `.test`, `.internal`, `.localhost`).
A real, internet-routable hostname is never certified, whatever a
`.fghj.yaml` says — so cloning a repo can never produce a trusted certificate
for a domain someone else owns. See [[host-aliases]] and
[[local-ca-and-tls-proxy]].

**It edits three system files, each inside a marked block it owns.**
`/etc/hosts` (between `# fghj-managed-begin`/`-end`), `/etc/resolver/<zone>`
files it can recognize as its own by parsing its own template, and one
`rdr-anchor "fghjd"` hook line in `/etc/pf.conf`. Each mechanism reads the
file, replaces only its own region, and preserves everything else verbatim.
`fghj daemon stop` removes all of them.

**The `pf` rule is the sharpest edge, and it is why the anchor is
top-level.** Two earlier designs were tried and abandoned. Nesting under
`com.apple/*` didn't reliably fire. Owning the top-level ruleset — reading
`/etc/pf.conf`, splicing in rules, reloading — worked, and then silently
erased Docker Desktop's *live-only* NAT rules on every tick, breaking all
published-port connectivity on the machine within a second of `fghjd`
starting. The rule that came out of that is the one stated in
[[two-zones-and-raw-ports]]: **fghjd must never be the one who breaks
someone else's networking**, even though someone else reloading pf is allowed
to transiently break fghjd. A dedicated top-level anchor can only ever
replace its own contents.

## What is deliberately *not* defended against

Stated plainly, so nobody mistakes silence for a guarantee:

- **A malicious repo in your workspace.** It can read your home directory via
  a bind mount and exfiltrate it from a container. Nothing stops this.
- **A repo claiming a hostname it doesn't own.** `additional_hosts:
  ["login.example.com"]` will route that name to localhost on your machine.
  It cannot get a certificate for it, so HTTPS fails rather than silently
  succeeding — but plain HTTP works.
- **A repo requesting `privileged: true`.** It is passed through to Docker.
  The schema's doc comment counsels against it; nothing enforces that.
- **Anything running as your user on the machine.** The control API listens
  on localhost with no authentication. Any local process can drive it.
- **A compromised base image.** fghj pulls what the config names.

## What *is* defended against

These are real properties, not aspirations:

- The CA can never mint a certificate for a name outside fghj's zone that
  isn't under a reserved TLD **and** currently routed.
- A repo cannot name a credential path outside its own checkout, or name
  whose ssh-agent to use.
- fghj's own container labels win over author-supplied `labels` on a key
  conflict, so a repo cannot break fghj's bookkeeping by colliding with it.
- fghj's edits to shared system files are confined to marked regions and are
  reversible.
- A derived domain can never be author-chosen, so one repo cannot claim
  another's address ([[node-identity-and-domains]]).

## Related

- [[persistence-and-workspace-store]] — the root/user privilege split.
- [[local-ca-and-tls-proxy]] — the CA and what it will sign.
- [[host-aliases]] — `cert_eligible` in full.
- [[build-inputs]] — why `ssh` is a boolean and not a path.
- [[two-zones-and-raw-ports]] — the pf anchor rule.

## Status

Described, not enforced by any single component — this file is a synthesis.
The individual mechanisms are implemented as cited above. No sandboxing of
bind mounts, `privileged`, or build steps exists or is planned; that is the
decision, not a gap.
