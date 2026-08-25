package fghj

#Build: {
	context:    string | *"."
	dockerfile: string | *"Dockerfile"
	args:       {[string]: string} | *{}
}

// An additional named HTTP surface on a service beyond its primary
// `http_port` — e.g. a Prometheus instance exposing both a scrape port and an
// admin UI port, each wanting its own `*.fghj.internal` name.
#HttpRoute: {
	port:   string
	domain: string & =~"^[a-z0-9-]+(\\.[a-z0-9-]+)*\\.fghj\\.internal$"
}

#Service: {
	name:            string & =~"^[a-z0-9][a-z0-9-]*$"
	internal_domain: string & =~"^[a-z0-9-]+(\\.[a-z0-9-]+)*\\.fghj\\.internal$"
	build:           #Build
	ports: [...string] & [_, ...]
	// Which declared port is the primary HTTP entrypoint reachable at
	// `internal_domain`. Defaults to the first entry in `ports` when omitted.
	http_port?: string
	// Extra HTTP surfaces on other ports, each getting its own domain.
	http_routes?: [...#HttpRoute]
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
