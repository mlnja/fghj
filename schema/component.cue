package fghj

// One `--mount=type=secret` source for a BuildKit build. `id` is what the
// Dockerfile names (`RUN --mount=type=secret,id=npmrc`); `file` is where the
// bytes come from, resolved against the repo checkout root exactly like
// `#Volume`'s bind `host` and `#RunOptions.env_file`.
//
// There is deliberately no `env:` variant, which BuildKit itself supports:
// `fghjd` is a root daemon with no access to the developer's shell
// environment, so there is no env to read one from. That is the same gap
// `concepts/AUDIT.md` E2 records, and it has to close first.
#BuildSecret: {
	id:   string & =~"^[a-zA-Z0-9][a-zA-Z0-9._-]*$"
	file: string
}

// A bare string is the context, as in Compose: `build: .`.
#Build: string | #BuildFull

#BuildFull: {
	// A path in this repo, or a git URL, `<url>#<ref>:<subdir>`, to build
	// someone else's code with your own definition. `ref` and `subdir` are
	// optional; a URL starts with `https://`, `http://`, `ssh://`, `git@`
	// or `file://`.
	// The clone lives in `.fghj/sources/` and is made by pull, never by
	// start. See `concepts/git-build-sources.md`.
	context:    string | *"."
	dockerfile: string | *"Dockerfile"
	// The Dockerfile itself, for a context that has none. Setting it and a
	// `dockerfile` other than the default is an error.
	dockerfile_inline?: string
	args:       {[string]: string} | *{}
	// Which stage of a multi-stage Dockerfile to build, `docker build
	// --target`. Omitted builds the final stage, as Docker does.
	target?: string & =~"^[a-zA-Z0-9][a-zA-Z0-9._-]*$"
	// Forward the workspace owner's ssh-agent into the build as BuildKit's
	// `default` socket, for a Dockerfile doing `RUN --mount=type=ssh` —
	// cloning a private sibling repo, a private Go module, a private Cargo
	// registry. The same agent fghj already forwards to `git clone` (see
	// `concepts/persistence-and-workspace-store.md`).
	ssh: bool | *false
	secrets: [...#BuildSecret] | *[]
}

// A single declared container port and its role. `primary` (at most one per
// service) puts it at the service's own derived domain; `name` gives it an
// additional nested domain `{name}.{service's domain}` — e.g. a Prometheus
// instance's scrape port as primary and its admin UI as a named extra.
// Neither set: still published to an ephemeral localhost port, just with no
// `*.fghj.internal` name — e.g. a raw TCP protocol the proxy can't route by
// Host/SNI. `name` is a bare label, like `#Service.name` — fghj derives the
// actual domain from it (scoped by workspace like everything else),
// there's no way to declare a raw domain here that would bypass that.
#Port: {
	primary: bool | *false
	name?:   string & =~"^[a-z0-9][a-z0-9-]*$"
	// Pin the host-side published port instead of letting Docker assign a
	// random ephemeral one — for protocols whose clients hardcode a port
	// number and can't go through name-based routing at all (raw MQTT, a
	// custom TCP protocol, etc). Only one container on the machine can hold
	// a given host port, so two workspaces pinning the same one can't run
	// at once.
	host_port?: uint & >0 & <=65535
	// When this port is `primary` and/or `name`d, also match every
	// subdomain of its derived domain, not just the exact name — same idea
	// as `#HostAlias.wildcard` below, but for the node's own auto-derived
	// `*.fghj.internal` domain, which an `#AdditionalHost` can never name
	// directly (see its doc comment). No effect on a port that's neither
	// `primary` nor `name`d — there's no domain to wildcard.
	wildcard: bool | *false
}

// A bind mount (host path) or a named volume (Docker-managed storage).
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
	// declaring node's id plus the workspace, the same way a node's domain
	// is. The node id is what keeps this label private to the node that
	// declared it: `data` in one repo and `data` in another are two
	// volumes, not one, exactly as two services both called `api` are two
	// nodes.
	name: string & =~"^[a-z0-9][a-z0-9-]*$"
	// Opt in to sharing this volume with any other node that declares the
	// same `name` + `shared: true`, anywhere in the workspace.
	// Drops the node-id qualification, so the label alone decides identity.
	//
	// Off by default, and deliberately awkward to reach for: two engines
	// with one data directory between them is silent corruption, and the
	// pair of configs that produce it can be written by two teams who have
	// never spoken. Sharing a *database* — the usual reason to want this —
	// needs none of it: services in one repo share its `postgres` by
	// depending on it, and another repo waits on a flow that publishes it.
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

// One entry under `services:` — your own code (`build`), a published image
// (`image`), or a task (something in this repo waits on it with
// `condition: service_completed_successfully`). Exactly one of `build` and
// `image`: fghj names the images it builds itself, so Compose's "both" (tag
// the build) has nothing to mean here.
#Service: {
	#RunOptions
	{build: #Build} | {image: string & =~"^[a-z0-9][a-z0-9._/-]*(:[a-zA-Z0-9._-]+)?$"}
	// A bare list of container port numbers, or a map of port number to
	// `#Port` — published to Docker as-is, not a semantic label.
	ports: [...string] | {[=~"^[0-9]+$"]: #Port} | *[]
	environment: #Environment | *[]
	// Overrides the image's default `CMD`, Compose-`command`-style. Empty
	// (the default) leaves the image's own `CMD`/`ENTRYPOINT` untouched. A
	// task must set it: a task *is* its command.
	command: [...string] | *[]
	volumes: [...#Volume] | *[]
	// Extra literal hostnames this service also answers on, routed to its
	// `primary` port — requires one to be set. See `#HostAlias`.
	additional_hosts: [...#HostAlias] | *[]
	// Compose's `depends_on`. See `#DependsOn`.
	depends_on: #DependsOn | *[]
	// Makes this service a task, like being waited on with
	// `service_completed_successfully` does — the way to declare a seed
	// that nothing waits on and only a flow lists. "on_start" (the
	// default) re-runs the task on every start and top-up — a migration,
	// whose command is idempotent. "once" runs it at most once per run,
	// and not again when the code changes; reach for it only when
	// re-running is expensive or destructive.
	//
	// A task can't set `healthcheck` (an exited container is never
	// healthy) or a `restart` other than "no" (it would restart forever).
	// The resolver refuses both.
	run?: "on_start" | "once"
}

// Another repo, by URL. The alias is how this file names it: `alias/flow`
// in a flow or `depends_on`, `alias` for all of it, and `alias/service`
// only inside `${FGHJ_SERVICE_FQDN:…}`. Resolved to the checkout on disk
// with that remote, whatever its folder is called; cloned into a folder
// named after the URL's last segment when it isn't there.
#Include: string & =~"^(git@|https://|ssh://)" | {
	repo: string & =~"^(git@|https://|ssh://)"
	// The branch fghj clones at the first fetch, `main` when omitted. Read
	// once, at clone time — after that the developer standing in the
	// checkout decides its branch. See `concepts/branch-ownership-model.md`.
	default_branch?: string
}

// A named start list — what to run when you don't want everything. Each
// entry is one of this repo's services, one of its flows, `alias/flow` (an
// included repo's flow), or `alias` (all of that repo and what it
// includes). Never another repo's service: its flows are its contract.
// Starting a flow starts its entries and everything they can't start
// without. See `concepts/flows-v2.md`.
#Flow: [...#Ref]

// A name in this repo, or `alias/name` in an included one.
#Ref: string & (=~"^[a-z0-9][a-z0-9-]*$" | =~"^[^/]+/[^/]+$")

#ComponentConfig: {
	// Any 2.x. Major is a compatibility barrier and minor is not: a
	// different major is refused, a newer minor accepted and noted, so one
	// repo can adopt a 2.1 feature while its peers stay on 2.0. See
	// `src/resolver/version.rs`.
	version: string & =~"^2\\.[0-9]+$"
	include?: [Alias=string & =~"^[a-z0-9][a-z0-9-]*$"]: #Include
	// Required (`!`), matching the daemon, which refuses a file without
	// it; an *empty* map stays legal, because the daemon accepts that too.
	services!: [Name=string & =~"^[a-z0-9][a-z0-9-]*$"]: #Service
	// No flows means the only start list is everything.
	flows?: [Name=string & =~"^[a-z0-9][a-z0-9-]*$"]: #Flow
}
