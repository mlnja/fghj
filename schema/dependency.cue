package fghj

// Docker Compose accepts `environment` as either a list of "KEY=value" strings
// or a map of KEY: value — mirrored here so backing/service env blocks read
// like a Compose fragment.
#Environment: {[string]: string} | [...string & =~"^[A-Za-z_][A-Za-z0-9_]*=.*$"]

// A dependency on another self-describing service — either cloned from
// another repo by URL, or (same pattern as #SharedBackingDependency) a
// sibling service already declared in this same repo's own `services:` map
// when `repo` is omitted. The cross-repo form resolves by cloning into the
// workspace under a folder named after the repo URL's last path segment —
// every dependent references the same repo the same way, by URL, so there's
// no per-dependent override to disagree about. The same-repo form has
// nothing to clone and no branch to pick — it's purely an ordering/
// healthcheck dependency between two services built from one checkout
// (e.g. a dev-server service that proxies to a backend service alongside
// it).
#GitDependency: {
	kind: "service"
	// Omit for the same-repo form (nothing to clone, no branch to pick);
	// required for the cross-repo form.
	repo?: string & =~"^(git@|https://|ssh://)"
	// The branch fghj clones this repo at the first time it fetches it
	// (`git clone --branch <it> --single-branch`); `main` when omitted. Its
	// job is that a dependency nobody on this machine has seen yet arrives
	// *ready to run*: the author declaring the edge is the one who knows
	// which branch of that repo works against theirs, so that's where it's
	// said. Read once, at clone time, and never again — re-pulling an
	// existing checkout only verifies its `origin` and never moves a live
	// working tree off the branch someone is on. So two edges naming
	// different branches for one repo is not a conflict: there is one
	// checkout per repo workspace-wide, and after the clone the only thing
	// deciding its branch is the developer standing in it. See
	// `concepts/branch-ownership-model.md`.
	default_branch?: string
	// Which of the target repo's #ComponentConfig.services this depends on —
	// one entry per service wanted, so depending on several services from the
	// same repo is still one dependency block (`repo`/`default_branch` stated
	// once, for the cross-repo form), not one block per service. Omit when
	// that repo declares exactly one service (used automatically); required
	// — and needs an entry per name — when it declares more than one, since
	// `repo` alone no longer names a single unambiguous node.
	services?: [...string & =~"^[a-z0-9][a-z0-9-]*$"]
}

// Mirrors Docker's own `HEALTHCHECK`/`HealthConfig`. `interval`/`timeout`/
// `start_period` are given in seconds here (converted to the nanoseconds
// Docker's API wants, at the `docker::run_container` boundary) so no CUE
// author has to think in nanoseconds. When declared, any node that depends
// on this one — a service depending on a backing dependency, or a service
// depending on another service — waits for it to report "healthy" before
// starting, instead of just "started"; see `runs::wait_for_healthy`.
#Healthcheck: {
	test: [...string] & [_, ...]
	interval?:     uint & >0
	timeout?:      uint & >0
	start_period?: uint & >0
	retries?:      uint & >0
}

// Docker Compose–parity knobs shared by both #Service and
// #BackingDependency — embedded into each via `& #RunOptions` rather than
// duplicated, since none of these differ between the two node kinds.
#RunOptions: {
	// Compose-equivalent restart policy, passed straight to Docker's
	// `HostConfig.RestartPolicy`. "no" (the default) leaves a stopped
	// container stopped — fghj's own `ensure_running` is the usual way a
	// container comes back, not Docker's own restart machinery.
	restart: *"no" | "always" | "on-failure" | "unless-stopped"
	// The signal Docker sends to stop this container, and how long it waits
	// for the process to exit before following up with SIGKILL — Compose's
	// `stop_signal`/`stop_grace_period`, Docker's
	// `ContainerCreateBody.StopSignal`/`StopTimeout`.
	//
	// Set on the container at *create* time rather than passed with each
	// stop call, so the policy travels with the container: an orphan whose
	// node was deleted from the graph, a container that outlived a daemon
	// restart, and a plain `docker stop` by hand all honour it.
	//
	// The default grace period is 10s, matching Docker's own. Raise it for
	// anything that needs to finish writing before it dies — a database
	// flushing to a `scope: stable` volume is the case this exists for. The
	// signal defaults to whatever the image declares (`STOPSIGNAL`, or
	// SIGTERM); override it only for an image whose process listens for a
	// different one (e.g. nginx's "quit" is SIGQUIT).
	stop_signal?:      string & =~"^SIG[A-Z0-9]+$"
	stop_grace_period: uint | *10
	// Overrides the image's default container user — Compose's `user`,
	// Docker's `ContainerCreateBody.User` (e.g. "1000:1000" or "postgres").
	user?: string
	// Overrides the image's default working directory — Compose's
	// `working_dir`, Docker's `ContainerCreateBody.WorkingDir`.
	working_dir?: string
	// Extra container labels, merged under fghj's own `com.docker.compose.*`
	// labels (see `docker::run_container`) — fghj's own labels always win on
	// a key conflict, so a user can't accidentally break fghj's own
	// container bookkeeping.
	labels: {[string]: string} | *{}
	// Linux capabilities to add/drop — Compose's `cap_add`/`cap_drop`,
	// Docker's `HostConfig.CapAdd`/`CapDrop`.
	cap_add: [...string] | *[]
	cap_drop: [...string] | *[]
	// Runs the container with extended (near-host-equivalent) privileges —
	// Compose's `privileged`. Defaults to `false`; only set this for a real,
	// specific need (e.g. a container that itself talks to the Docker
	// daemon), same caution Compose's own docs give.
	privileged: bool | *false
	// Extra literal `hostname:IP` entries written into the container's own
	// `/etc/hosts` — Compose's `extra_hosts`, Docker's
	// `HostConfig.ExtraHosts`. Distinct from `#Service.additional_hosts`:
	// this is the container resolving *something else*, not the host
	// resolving *this* container.
	extra_hosts: [...string & =~"^[^:]+:.+$"] | *[]
	healthcheck?: #Healthcheck
	// Pins the platform (Docker's `os[/arch[/variant]]`, e.g. "linux/amd64").
	// On a #Service this targets `docker build --platform`, for cross-
	// compiling the service's own image to a specific architecture. On a
	// #BackingDependency it targets `create_container`'s platform-aware image
	// lookup, for a backing image only published for one architecture. Unset
	// (the default) lets Docker pick the host's own platform, same as today.
	platform?: string
	// `.env`-style files loaded before `environment`, Compose's `env_file`.
	// On a #Service, each path resolves against this repo's own checkout
	// root, same rule as `#Volume.host`. On a #BackingDependency, there's no
	// checkout of its own, so each path resolves against the *owning*
	// service's checkout root instead — the service whose .fghj.yaml declares
	// this dependency inline, same as Compose resolving `env_file` against
	// the compose file's own directory regardless of `build` vs `image`.
	// Declared entries are loaded in order, then `environment` is applied on
	// top, so an explicit `environment` entry always wins over one loaded
	// from a file.
	env_file: [...string] | *[]
}

// A dependency on a backing service (a datastore, broker, etc. — the 12-Factor
// App sense: any service consumed over the network that isn't code you own)
// provisioned directly from an image — nothing to clone, no .fghj.yaml of its
// own. This service *owns* the resource: it's the one instance that
// `#SharedBackingDependency` refs point at.
#BackingDependency: {
	#RunOptions
	kind:  "backing"
	name:  string & =~"^[a-z0-9][a-z0-9-]*$"
	image: string & =~"^[a-z0-9][a-z0-9._/-]*(:[a-zA-Z0-9._-]+)?$"
	environment: #Environment | *[]
	// Overrides the image's default `CMD` — e.g. `["mysqld", "--sql_mode=..."]`
	// to customize a stock database image's startup flags without a custom
	// Dockerfile. Empty (the default) leaves the image's own `CMD` untouched.
	command: [...string] | *[]
	// A bare list of port numbers (each implicitly non-primary, unnamed —
	// e.g. a plain postgres/mysql with nothing worth routing HTTP to) or a
	// map of port number to `#Port` config, exactly like `#Service.ports` —
	// for a backing dependency that itself exposes more than one port with
	// different roles (e.g. minio's S3 API + web console, or
	// grafana/prometheus/victoriametrics exposing both a UI and a
	// scrape/write endpoint), so one can be `primary` and/or another `name`d
	// instead of every port being an equally-anonymous published localhost
	// port.
	ports: [...string] | {[=~"^[0-9]+$"]: #Port}
	// Every node's domain is derived by fghj, never author-declared (see
	// #Service.name) — this just picks whether the derived name carries the
	// run id. "run" (the default) scopes it to the run that started it —
	// e.g. a preview run's postgres never collides with the default run's,
	// since each run gets its own docker network and its own name. "stable"
	// drops the run from the name, giving this dependency one fixed identity
	// shared across every run of this graph — only one run can own that name
	// from the host at a time, but it's the same name every time.
	domain_scope: *"run" | "stable"
	volumes: [...#Volume] | *[]
}

// A reference to a #BackingDependency owned by another service already present
// in the resolved flow graph — binds to that same running instance instead of
// provisioning a second one. Identifies the owning service by `repo` + `service`
// — a bare service name alone isn't unique (two peer repos, or two services in
// the same repo, can share a name), so both are needed to pick one unambiguous
// node. Omit `repo` to reference a sibling service declared in this same repo's
// `services:` map (e.g. two independently-built services sharing one database).
// `repo` (when given) + `service` + `name` must match another service's
// declared #BackingDependency exactly; the resolver rejects dangling references.
#SharedBackingDependency: {
	kind:    "shared-backing"
	repo?:   string & =~"^(git@|https://|ssh://)"
	service: string & =~"^[a-z0-9][a-z0-9-]*$"
	name:    string & =~"^[a-z0-9][a-z0-9-]*$"
}

// A node that is *supposed to exit* — a database seed, a schema migration, a
// fixture loader. The third node kind with a lifecycle, and it needs to be a
// kind rather than a flag on #Service because `exited` means opposite things
// for the two: for a service it is drift, for a task it is success. Nothing
// downstream (the reconciler, the UI's status badge, `ensure_running`) can
// tell those apart without knowing which kind it is looking at.
//
// Declared inline by the service that needs it, exactly like
// #BackingDependency — the task is owned by that service, and the service
// does not start until the task has exited 0. A task that exits non-zero
// blocks its dependents rather than letting them start against a
// half-migrated database.
#Task: {
	#RunOptions
	kind: "task"
	name: string & =~"^[a-z0-9][a-z0-9-]*$"
	// Omit to run the *owning service's own built image* — the usual case,
	// since a migration or seed is almost always that service's code with a
	// different command (`rake db:migrate`, `alembic upgrade head`). Give an
	// image only for a task that genuinely isn't the owner's code, e.g. a
	// stock `postgres:16` running `psql -f fixtures.sql`. The owning service
	// must declare a `build` if this is omitted — there is nothing to
	// inherit otherwise.
	image?: string & =~"^[a-z0-9][a-z0-9._/-]*(:[a-zA-Z0-9._-]+)?$"
	// Required, and the reason a task exists: a task *is* its command. A
	// container with no command to run would just re-run the image's own
	// long-running `CMD`, which is a service, not a task.
	command: [...string] & [_, ...]
	environment: #Environment | *[]
	volumes: [...#Volume] | *[]
	// Order this task after other dependencies of the *same owning service*,
	// named the way they name themselves: a sibling #BackingDependency's or
	// #Task's `name`, or a sibling `kind: service` dependency's service name.
	// A seed needs its database healthy first, and without this the only
	// guaranteed ordering is "before the owner", which is not enough.
	//
	// Scoped to siblings deliberately. Ordering against an arbitrary node
	// elsewhere in the workspace would be an edge between two repos that
	// never agreed to one, which is the coupling [[flat-workspace-model]]
	// exists to prevent.
	after: [...string & =~"^[a-z0-9][a-z0-9-]*$"] | *[]
	// "on_start" (the default) re-runs this task on every start and every
	// top-up, which is what a migration wants: the task's own command is
	// expected to be idempotent. "once" runs it at most once per run, for
	// the expensive or destructive case.
	//
	// Note what "once" costs: it means *at most once per run*, full stop.
	// It is not re-run when the code changes — a `git pull` that adds a
	// migration will not cause a "once" task to run again, even though fghj
	// can see that the commit moved. Reach for it only when re-running is
	// expensive or destructive; that is why the default is the other way
	// round.
	run: *"on_start" | "once"
	// A task's completion predicate is its exit code. A healthcheck is
	// meaningless — an exited container can never report Docker-`healthy`,
	// which is exactly the hole this kind fills — so declaring one here is
	// an error rather than a silently ignored field.
	healthcheck?: _|_
	// Forced, not defaulted. A restart policy on a container whose whole
	// purpose is to exit would restart it forever.
	restart: "no"
}

#Dependency: #GitDependency | #BackingDependency | #SharedBackingDependency | #Task
