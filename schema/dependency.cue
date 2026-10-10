package fghj

// Docker Compose accepts `environment` as either a list of "KEY=value" strings
// or a map of KEY: value — mirrored here so backing/service env blocks read
// like a Compose fragment.
#Environment: {[string]: string} | [...string & =~"^[A-Za-z_][A-Za-z0-9_]*=.*$"]

// Compose's `depends_on`: a list of names, or a map of name to the long
// form. A name is a service in this repo, or — across repos — `alias/flow`
// or `alias`. Waiting on another repo's flow waits on each of its members.
#DependsOn: [...#Ref] | {[#Ref]: #DependsOnEntry | null}

#DependsOnEntry: {
	// A required edge waits for the target to be ready: healthy if it has
	// a healthcheck, exited 0 if it's a task, running otherwise. So the
	// condition is checked against the target rather than changing the
	// wait — `service_healthy` needs a healthcheck there, and
	// `service_completed_successfully` makes the target a task (same repo
	// only: another repo's flow is never "completed").
	condition: *"service_started" | "service_healthy" | "service_completed_successfully"
	// `true`: needed to start. The target starts first, is waited on, and
	// comes into every run this service is in; if it fails, this service
	// is blocked; if it's recreated or stopped, so is this service.
	// `false`: needed at runtime. Nothing is ordered or waited on, and the
	// target starts only if a flow lists it. `service_completed_successfully`
	// can't be combined with it. See concepts/dependency-kinds.md.
	required: bool | *true
}

// Mirrors Docker's own `HEALTHCHECK`/`HealthConfig`. `interval`/`timeout`/
// `start_period` are given in seconds here (converted to the nanoseconds
// Docker's API wants, at the `docker::run_container` boundary) so no CUE
// author has to think in nanoseconds. When declared, anything that depends
// on this service waits for it to report "healthy" before starting, instead
// of just "started"; see `runs::wait_for_healthy`.
#Healthcheck: {
	test: [...string] & [_, ...]
	interval?:     uint & >0
	timeout?:      uint & >0
	start_period?: uint & >0
	retries?:      uint & >0
}

// Docker Compose–parity knobs, embedded into #Service.
#RunOptions: {
	// Compose-equivalent restart policy, passed straight to Docker's
	// `HostConfig.RestartPolicy`. "no" (the default) leaves a stopped
	// container stopped — fghj's own `ensure_running` is the usual way a
	// container comes back, not Docker's own restart machinery.
	restart?: "no" | "always" | "on-failure" | "unless-stopped"
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
	// flushing to a named volume is the case this exists for. The
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
	// With `build` it targets `docker build --platform`; with `image`,
	// `create_container`'s platform-aware image lookup, for an image only
	// published for one architecture. Unset lets Docker pick the host's.
	platform?: string
	// `.env`-style files loaded before `environment`, Compose's `env_file`.
	// Each path resolves against this repo's checkout root, same rule as
	// `#Volume.host` — the equivalent of Compose's "the compose file's
	// directory", whether the service has `build` or `image`.
	// Declared entries are loaded in order, then `environment` is applied on
	// top, so an explicit `environment` entry always wins over one loaded
	// from a file.
	env_file: [...string] | *[]
	// The port this node's debugger listens on inside the container, e.g.
	// 9229 for Node's inspector, 5678 for debugpy, 2345 for Delve.
	//
	// Declaring it does one thing: the port is published like any other
	// declared port, so it answers at `{node's raw domain}:{this port}`
	// from the host *and* from inside the run's network, on the declared
	// number — see `concepts/two-zones-and-raw-ports.md`. That is the
	// address you point an IDE at.
	//
	// Nothing is injected into the container's environment. This is a port
	// declaration, exactly like `ports`, and behaves like one: your app
	// already knows which port it listens on, and this tells fghj so it can
	// publish and address it. Starting the debugger is your image's job —
	// see `guides/debugging.md` for a recipe per language. An image that
	// never listens makes declaring `debug` inert, which is a harmless
	// no-op rather than an error.
	//
	// What `debug` buys over just putting the number in `ports` is the
	// *meaning*: fghj knows which port is the debugger, so the UI can label
	// the address and offer the halt-at-startup switch for this node.
	//
	// Always set, not toggled: a debugger that is merely *listening* costs
	// essentially nothing and cannot stop anything, because a breakpoint
	// only exists once an IDE has attached and sent it. Halting at startup
	// is the one thing that does need a switch, and that's
	// `FGHJ_DEBUG_WAIT` — runtime state flipped per container from the UI,
	// never declared here, because it must not be committed for a whole
	// team (see `concepts/debugging-in-containers.md`).
	debug?: uint & >0 & <=65535
}
