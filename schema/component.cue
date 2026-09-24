package fghj

#Build: {
	context:    string | *"."
	dockerfile: string | *"Dockerfile"
	args:       {[string]: string} | *{}
}

// A single declared container port and its role. `primary` (at most one per
// service) puts it at the service's own derived domain; `name` gives it an
// additional nested domain `{name}.{service's domain}` — e.g. a Prometheus
// instance's scrape port as primary and its admin UI as a named extra.
// Neither set: still published to an ephemeral localhost port, just with no
// `*.fghj.internal` name — e.g. a raw TCP protocol the proxy can't route by
// Host/SNI. `name` is a bare label, like `#Service.name` — fghj derives the
// actual domain from it (scoped by workspace and run like everything else),
// there's no way to declare a raw domain here that would bypass that.
#Port: {
	primary: bool | *false
	name?:   string & =~"^[a-z0-9][a-z0-9-]*$"
	// Pin the host-side published port instead of letting Docker assign a
	// random ephemeral one — for protocols whose clients hardcode a port
	// number and can't go through name-based routing at all (raw MQTT, a
	// custom TCP protocol, etc). This is the same trade-off as
	// `#BackingDependency.domain_scope: "stable"`: an explicit, conscious
	// opt-out of per-run isolation — only one run can hold this exact host
	// port at a time, so starting a second run with the same fixed port
	// will fail to bind rather than silently getting its own copy.
	host_port?: uint & >0 & <=65535
	// When this port is `primary` and/or `name`d, also match every
	// subdomain of its derived domain, not just the exact name — same idea
	// as `#HostAlias.wildcard` below, but for the node's own auto-derived
	// `*.fghj.internal` domain, which an `#AdditionalHost` can never name
	// directly (see its doc comment). No effect on a port that's neither
	// `primary` nor `name`d — there's no domain to wildcard.
	wildcard: bool | *false
}

// A bind mount (host path) or a named volume (Docker-managed storage), on
// either a #Service or a #BackingDependency.
#Volume: {
	container: string
	read_only: bool | *false
} & ({
	// Bind mount: a host path, resolved against the declaring repo's
	// checkout root if relative. Not sandboxed — an absolute or
	// `..`-escaping path passes straight through to Docker, same as Compose.
	host: string
} | {
	// Named volume: a bare label, like `#Port.name` — the real Docker
	// volume name is derived (never author-declared), folding in the
	// declaring node's id plus workspace/run, the same way a node's domain
	// is. The node id is what keeps this label private to the node that
	// declared it: `data` in one repo and `data` in another are two
	// volumes, not one, exactly as two services both called `api` are two
	// nodes.
	name: string & =~"^[a-z0-9][a-z0-9-]*$"
	// Same semantics as `domain_scope` below: "run" (the default) folds the
	// run id into the derived volume name, so a preview run gets its own
	// fresh empty storage. "stable" drops it, giving the volume one fixed
	// identity shared across every run.
	scope: *"run" | "stable"
	// Opt in to sharing this volume with any other node that declares the
	// same `name` + `scope` + `shared: true`, anywhere in the workspace.
	// Drops the node-id qualification, so the label alone decides identity.
	//
	// Off by default, and deliberately awkward to reach for: two engines
	// with one data directory between them is silent corruption, and the
	// pair of configs that produce it can be written by two teams who have
	// never spoken. Sharing a *backing dependency* — the usual reason to
	// want this — is already expressible as #SharedBackingDependency, which
	// gives you one node and therefore one volume without any of this.
	shared: bool | *false
})

// A literal hostname alias for a service, alongside its derived
// `*.fghj.internal` domain — e.g. a pre-existing OAuth callback hostname a
// third party already has on file. Must be an ordinary multi-label hostname,
// and can never sit inside fghj's own zone (that domain is always *derived*,
// never author-declared — see `#Port`'s doc comment for the same rule).
// Whether it gets HTTPS via fghj's local CA depends on whether it's under an
// IANA reserved special-use TLD (`.local`, `.test`, `.internal`,
// `.localhost`) — see `dns::is_reserved_alias`. Anything else is treated as
// a real, potentially internet-routable hostname: proxied over plain HTTP
// only, never certified, so fghj's system-trusted CA can never mint a cert
// for a domain it doesn't actually own.
#AdditionalHost: string &
	=~"^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$" &
	!~"(^|\\.)fghj\\.internal$"

// An `#AdditionalHost`, or the same host with an explicit wildcard toggle:
// a bare string is exact-match only, `{host: ..., wildcard: true}` also
// matches every subdomain of it — e.g. "myservice.local" as a bare string
// only matches that exact name; wildcarded, it also matches
// `acme.myservice.local` and `microsoft.myservice.local` alike, without
// either needing to be declared up front. For a tenant-per-subdomain app,
// this is what lets arbitrarily many tenant hostnames route to the same
// `primary` port locally, the same way they'd all hit the same backend in
// prod. `/etc/hosts` has no wildcard syntax, so a wildcarded entry resolves
// only through fghjd's own DNS server (and, on macOS, `/etc/resolver`); the
// same reserved-TLD rule (`.local`/`.test`/`.internal`/`.localhost`)
// decides whether it gets a real HTTPS cert or plain-HTTP-only either way.
#HostAlias: #AdditionalHost | {
	host:     #AdditionalHost
	wildcard: bool | *false
}

#Service: {
	#RunOptions
	build: #Build
	// Keyed by the literal container port number (e.g. "8080"), published to
	// Docker as-is — not a semantic label. `#Port.name` is where a label
	// belongs.
	ports: [=~"^[0-9]+$"]: #Port
	// "run" (the default) scopes this service's domain to the run that
	// started it, same as every other node — two runs of this service never
	// collide. "stable" drops the run id, giving it one fixed identity
	// shared across every run of this graph, the same trade-off as
	// `#BackingDependency.domain_scope: "stable"`: only one run can own that
	// name from the host at a time, but it's the same name every time.
	domain_scope: *"run" | "stable"
	environment: #Environment | *[]
	// Overrides the image's default `CMD`, Compose-`command`-style — e.g.
	// passing extra flags to a database's entrypoint script. Empty (the
	// default) leaves the image's own `CMD`/`ENTRYPOINT` untouched.
	command: [...string] | *[]
	volumes: [...#Volume] | *[]
	// Extra literal hostnames this service also answers on, routed to its
	// `primary` port — requires one to be set. Each entry is a bare
	// hostname (exact match) or `{host: ..., wildcard: true}` (also matches
	// every subdomain of it). See `#HostAlias`.
	additional_hosts: [...#HostAlias] | *[]
	dependencies: [...#Dependency]
}

// A user-facing journey through the graph, e.g. "simple login flow". Any repo
// may declare zero or more of these — there is no distinguished "root" repo.
#Flow: {
	description: string
	// Which of this repo's #services this flow is rooted at. Omit when the
	// repo declares exactly one service (it's used automatically); required
	// when it declares more than one, since there's no other way to tell
	// which service's dependencies the flow is actually describing.
	service?: string & =~"^[a-z0-9][a-z0-9-]*$"
	dependencies: [...#Dependency] & [_, ...]
}

#ComponentConfig: {
	// Any 1.x. fghj treats major as a compatibility barrier and minor as
	// not one: a different major is refused outright, while a newer minor
	// is accepted and merely noted, so one repo can adopt a 1.1 feature
	// while its peers stay on 1.0 and they still resolve together. Pinning
	// the literal "1.0" here would reject a file the daemon accepts, which
	// would make this schema lie to the person running `fghj validate` and
	// would put back the flag-day coordination the federated model exists
	// to avoid. See `src/resolver/version.rs`.
	version: string & =~"^1\\.[0-9]+$"
	// Keyed by service name (was a singular `service:` field) — a repo can
	// build more than one independent container from its own source (e.g. a
	// dev-server process and a backend API process, each with their own
	// Dockerfile), each with its own dependencies. See #SharedBackingDependency
	// for how two services in the same repo (or different repos) can share
	// one `kind: backing` instance instead of each provisioning their own.
	services: [Name=string & =~"^[a-z0-9][a-z0-9-]*$"]: #Service
	flows: [string]: #Flow
}
