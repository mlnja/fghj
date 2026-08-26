package fghj

// Docker Compose accepts `environment` as either a list of "KEY=value" strings
// or a map of KEY: value — mirrored here so backing/service env blocks read
// like a Compose fragment.
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

// A dependency on a backing service (a datastore, broker, etc. — the 12-Factor
// App sense: any service consumed over the network that isn't code you own)
// provisioned directly from an image — nothing to clone, no fghj.yaml of its
// own. This service *owns* the resource: it's the one instance that
// `#SharedBackingDependency` refs point at.
#BackingDependency: {
	kind:        "backing"
	name:        string & =~"^[a-z0-9][a-z0-9-]*$"
	image:       string & =~"^[a-z0-9][a-z0-9._/-]*(:[a-zA-Z0-9._-]+)?$"
	environment: #Environment | *[]
	ports: [...string]
	// Every node's domain is derived by fghj, never author-declared (see
	// #Service.name) — this just picks whether the derived name carries the
	// run id. "run" (the default) scopes it to the run that started it —
	// e.g. a preview run's postgres never collides with the default run's,
	// since each run gets its own docker network and its own name. "stable"
	// drops the run from the name, giving this dependency one fixed identity
	// shared across every run of this graph — only one run can own that name
	// from the host at a time, but it's the same name every time.
	domain_scope: *"run" | "stable"
}

// A reference to a #BackingDependency owned by another service already present
// in the resolved flow graph — binds to that same running instance instead of
// provisioning a second one. Identifies the owning service by `repo` (same as
// #GitDependency), not by its declared #Service.name — that name alone isn't
// unique across peer repos (see #Service.name), while `repo` is. `repo` +
// `name` must match another service's declared #BackingDependency exactly;
// the resolver rejects dangling references.
#SharedBackingDependency: {
	kind: "shared-backing"
	repo: string & =~"^(git@|https://|ssh://)"
	name: string & =~"^[a-z0-9][a-z0-9-]*$"
}

#Dependency: #GitDependency | #BackingDependency | #SharedBackingDependency
