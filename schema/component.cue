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
}

#Service: {
	name:  string & =~"^[a-z0-9][a-z0-9-]*$"
	build: #Build
	ports: [string]: #Port
	// "run" (the default) scopes this service's domain to the run that
	// started it, same as every other node — two runs of this service never
	// collide. "stable" drops the run id, giving it one fixed identity
	// shared across every run of this graph, the same trade-off as
	// `#BackingDependency.domain_scope: "stable"`: only one run can own that
	// name from the host at a time, but it's the same name every time.
	domain_scope: *"run" | "stable"
	environment: #Environment | *[]
	dependencies: [...#Dependency]
}

// A user-facing journey through the graph, e.g. "simple login flow". Any repo
// may declare zero or more of these — there is no distinguished "root" repo.
#Flow: {
	description: string
	dependencies: [...#Dependency] & [_, ...]
}

#ComponentConfig: {
	version: "1.0"
	service: #Service
	flows: [string]: #Flow
}
