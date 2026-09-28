# Host aliases: names fghj does not own

## The problem

[[node-identity-and-domains]] is about names fghj *derives*, and the rule
there is absolute: the author never picks a domain, because two CUE authors
who never talk to each other would eventually pick the same one.

That rule has a cost. Some hostnames are not fghj's to choose. A third party
has an OAuth callback URL on file. A tenant-per-subdomain app routes by
`Host:` and expects `acme.myapp.test` and `microsoft.myapp.test` to reach the
same service. A legacy service has a hardcoded hostname in a config file
nobody wants to touch. None of these can be `*.fghj.internal`, and none of
them can be made up by fghj.

`#Service.additional_hosts` is the escape hatch: a list of literal hostnames
that route to this service's primary port, alongside its derived domain.

## Three rules that make the hatch safe

**A host alias can never be inside fghj's own zone.** The CUE pattern ends
with `!~"(^|\\.)fghj\\.internal$"`, so an author cannot hand-pick a name in
the zone the derivation owns. The whole argument for derived domains
collapses if an alias can claim one — and the collision would be invisible,
since `resolve_route` scans one flat namespace.

**It must be a real, multi-label hostname.** The pattern requires at least
one dot. A single-label alias would compete with the OS's own search-domain
behaviour and with every unqualified name on the machine.

**It only ever gets a certificate if fghj could plausibly own the name.**
This is the important one. `dns::cert_eligible` is the single rule:

```rust
in_zone(name) || (is_reserved_alias(name) && routed)
```

An in-zone `*.fghj.internal` name always qualifies — that is the zone this
tool exists to serve. Anything else qualifies only if it is under an IANA
reserved, never-real TLD (`.local`, `.test`, `.internal`, `.localhost`)
**and** is actually routed to a running container right now. Everything else
— a genuine, internet-routable hostname — is proxied over plain HTTP and
never certified.

fghj's CA is in the system trust store. A cert it mints is trusted by every
browser on the machine. If it would sign `login.realcompany.com` because
someone wrote that string in a `.fghj.yaml`, then cloning a repo would be
enough to make its author's proxy indistinguishable from the real site for
that user. The reserved-TLD restriction means the CA can only ever certify
names that are guaranteed by IANA not to exist on the public internet. The
`routed` conjunct closes the other half: without it, any `.local`-shaped SNI
arriving at the proxy would mint trust for a name nothing in the workspace
declared.

`start_node` uses the same function to tell the UI whether a route can be
linked as `https://` at all, so the badge in the drawer and the cert
resolver can never disagree about a name.

## Wildcards, and why they need a different mechanism

A bare string in `additional_hosts` is exact-match only. `{host: "...",
wildcard: true}` also matches every subdomain, which is what makes a
tenant-per-subdomain app work locally the way it works in prod: arbitrarily
many tenant hostnames reach the same primary port without any of them being
declared up front.

That distinction forces two different delivery mechanisms, because
`/etc/hosts` has no wildcard syntax.

- **Exact aliases** go into `/etc/hosts`, as one `127.222.0.1 <host>` line each,
  inside a marked block (`# fghj-managed-begin` / `-end`). Everything outside
  the markers — the user's own entries, macOS's default `localhost` line — is
  preserved byte for byte. The rewrite is a full-block replacement, sorted
  and deduped, so repeated syncs of the same logical set never produce a
  spurious file change. `main.rs`'s `daemon_stop` can strip the block via
  `sudo` without `fghjd` being alive, the same way it cleans up the pidfile
  and resolver files.
- **Wildcarded aliases** resolve only through fghjd's own DNS server. On
  macOS that means `install_os_resolver_config` writes an `/etc/resolver`
  file for the suffix, which is why that function takes a live
  `wildcard_zones` list and runs on every reconcile tick rather than once at
  startup ([[split-dns]]). On a platform without `/etc/resolver`, wildcard
  aliases simply don't resolve — there is no `/etc/hosts` fallback that could
  express them.

The one CUE field splits into two Rust fields at resolve time —
`Node.additional_hosts` holds the bare entries, `Node.wildcard_hosts` the
wildcarded ones — because past that point they are never handled together
again.

Both share the lifecycle of a `state::PortRoute`: the claim exists exactly
while a container claiming it is *running*. Stop the container and the
`/etc/hosts` line and the resolver file both go away on the next tick.

## An alias is a second name for the primary port

An alias does not get its own port. `start_node` finds the route it already
built for the node's own derived domain and attaches every alias to the same
host port — the same "several names, one backend port" shape a named port
already has, just keyed off a literal author-declared hostname instead of a
derived one.

The corollary is that a node with no `primary` port has nothing for an alias
to attach to, and the aliases are then *silently dropped* rather than
erroring. That is deliberate but only defensible because
`resolver::check_ports` already warns about exactly this case at
graph-resolution time, where the author can see it — failing a container
start over it would surface the same problem later, less clearly, and after
work had already been done.

There is a third wildcard knob, easy to confuse with the other two:
`#Port.wildcard` applies the same subdomain-matching behaviour to the node's
*derived* domain, which an alias can never name directly. It is the way to
get `*.{node}.{workspace}.fghj.internal` without leaving fghj's own zone, and
it has no effect on a port that is neither `primary` nor `name`d, because
then there is no domain to wildcard.

## Exact beats wildcard, across workspaces

`state::query::resolve_route` is deliberately two passes over every wired
workspace rather than one pass that prefers exact matches within each.

The reason is the cross-workspace case. If it were one pass, a wildcard
declared in the *first* workspace scanned would win over an exact match in
the last, purely because of iteration order — and iteration order is a
`BTreeMap` implementation detail nobody declared. Two passes make the
precedence a property of the rule rather than of the scan: every exact claim
everywhere loses to nothing, and a wildcard only ever fills in for a name
that nothing more specific has claimed.

Suffix matching is a real suffix check, not a substring one:
`notmyservice.local` must not match `myservice.local`, so `matches_suffix`
tests `host == suffix || host.ends_with(&format!(".{suffix}"))`.

## What is still unchecked

An author can claim a real hostname they do not own. `resolver::uniqueness`
catches an alias colliding with a derived domain or another alias inside the
workspace, but nothing can catch `additional_hosts: ["login.github.com"]` —
fghj has no way to know who owns a name. What it *can* do, and does, is
refuse to certify it: the alias resolves to localhost and is proxied over
plain HTTP, so a browser visiting it over HTTPS fails rather than silently
trusting a local impostor. That is the boundary between "this is your
machine, you cloned this repo" and "this tool will forge trust for you."

## Related

- [[node-identity-and-domains]] — the derived names, and the projection
  table an alias joins.
- [[local-ca-and-tls-proxy]] — what `cert_eligible` gates.
- [[split-dns]] — how a wildcard suffix becomes an OS resolver route.
- [[two-zones-and-raw-ports]] — aliases live in the http zone only; a raw
  port has no name to alias.

## Status

Implemented: `schema/component.cue` (`#AdditionalHost`, `#HostAlias`),
`src/hosts_file.rs` (the marked `/etc/hosts` block),
`src/effects/hosts.rs` (the fanned-in effect that collects aliases from every
running container in every workspace), `src/state/query.rs`
(`resolve_route`'s two passes, `wildcard_suffixes`), `src/dns.rs`
(`is_reserved_alias`, `cert_eligible`, wildcard zone install),
`src/resolver/uniqueness.rs` (in-workspace collisions). macOS only for
wildcards.
