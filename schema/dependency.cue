package fghj

// Docker Compose accepts `environment` as either a list of "KEY=value" strings
// or a map of KEY: value — mirrored here so infra/service env blocks read like
// a Compose fragment.
#Environment: {[string]: string} | [...string & =~"^[A-Za-z_][A-Za-z0-9_]*=.*$"]

// A dependency on another self-describing service repo, resolved by cloning it
// into the workspace under a folder named after the repo URL's last path
// segment — every dependent references the same repo the same way, by URL,
// so there's no per-dependent override to disagree about.
#GitDependency: {
	kind:           "service"
	repo:           string & =~"^(git@|https://|ssh://)"
	default_branch: string
}

// A dependency on unmanaged infrastructure (a datastore, broker, etc.) provisioned
// directly from an image — nothing to clone, no fghj.yaml of its own. This service
// *owns* the resource: it's the one instance that `#SharedInfraDependency` refs
// point at.
#InfraDependency: {
	kind:        "infra"
	name:        string & =~"^[a-z0-9][a-z0-9-]*$"
	image:       string & =~"^[a-z0-9][a-z0-9._/-]*(:[a-zA-Z0-9._-]+)?$"
	environment: #Environment | *[]
	ports: [...string]
}

// A reference to an #InfraDependency owned by another service already present
// in the resolved flow graph — binds to that same running instance instead of
// provisioning a second one. `service` + `name` must match another service's
// declared #InfraDependency exactly; the resolver rejects dangling references.
#SharedInfraDependency: {
	kind:    "shared-infra"
	service: string & =~"^[a-z0-9][a-z0-9-]*$"
	name:    string & =~"^[a-z0-9][a-z0-9-]*$"
}

#Dependency: #GitDependency | #InfraDependency | #SharedInfraDependency
