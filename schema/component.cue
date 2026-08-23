package fghj

#Build: {
	context:    string | *"."
	dockerfile: string | *"Dockerfile"
	args:       {[string]: string} | *{}
}

#Service: {
	name:            string & =~"^[a-z0-9][a-z0-9-]*$"
	internal_domain: string & =~"^[a-z0-9-]+(\\.[a-z0-9-]+)*\\.fghj\\.internal$"
	build:           #Build
	ports: [...string] & [_, ...]
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
