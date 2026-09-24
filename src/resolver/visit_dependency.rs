//! Resolving one declared dependency into nodes and edges — a git
//! dependency on another repo, a sibling service, or a backing dependency.

//! `ResolveCtx` — the traversal that walks components and their
//! dependencies, turning parsed config into graph nodes and edges.

use std::collections::BTreeMap;

use super::config::{default_domain_scope, default_restart};
use super::dependency::{BackingDependencyConfig, Dependency};
use super::graph::{Edge, Node};
use super::repo_url::{normalize_repo_url, repo_name_from_url};

use super::visit::ResolveCtx;
use super::warning::Warning;

impl<'a> ResolveCtx<'a> {
    /// Resolves a `Dependency::Service` reference to its conventional local
    /// path, then either recurses into it (if already on disk) or registers a
    /// `downloaded: false` stub node (if not) — never clones. `wanted_service`
    /// picks which of the target repo's declared services this depends on
    /// (see `visit_local_service`); returns `None` (after pushing a warning)
    /// if that's ambiguous or unresolvable, without creating a `depends-on`
    /// edge at all.
    pub(crate) fn visit_service_dependency(
        &mut self,
        owner_id: &str,
        repo: &str,
        default_branch: Option<&str>,
        wanted_service: Option<&str>,
    ) -> Option<String> {
        let norm = normalize_repo_url(repo);

        // An on-disk match by the repo's real git remote wins over the plain
        // convention-derived name — matters if the repo was cloned by hand
        // (or via a different URL form) before fghj ever touched it.
        let local_path = self
            .repo_index
            .get(&norm)
            .cloned()
            .unwrap_or_else(|| repo_name_from_url(repo));

        let child_id = if let Some(component) = self.scanned.get(&local_path) {
            self.visit_local_service(&local_path, component, wanted_service)?
        } else {
            // Not on disk yet: register a stub, deduped by repo url so the
            // same not-yet-pulled repo referenced with different local_path
            // spellings still renders as a single node. The stub's id is
            // whichever local_path was seen first (its real service name is
            // unknown until pulled).
            let stub_id = self
                .stub_repo_ids
                .entry(norm.clone())
                .or_insert_with(|| local_path.clone())
                .clone();
            self.nodes.entry(stub_id.clone()).or_insert_with(|| Node {
                id: stub_id.clone(),
                label: stub_id.clone(),
                kind: "service".into(),
                image: None,
                branch: default_branch.map(str::to_string),
                repo: Some(repo.to_string()),
                domain_scope: default_domain_scope(),
                local_path: Some(stub_id.clone()),
                domain: String::new(),
                downloaded: false,
                dirty: false,
                flows: Vec::new(),
                build: None,
                ports: BTreeMap::new(),
                environment: Vec::new(),
                command: Vec::new(),
                volumes: Vec::new(),
                additional_hosts: Vec::new(),
                wildcard_hosts: Vec::new(),
                env_file: Vec::new(),
                restart: default_restart(),
                user: None,
                working_dir: None,
                labels: BTreeMap::new(),
                cap_add: Vec::new(),
                cap_drop: Vec::new(),
                privileged: false,
                extra_hosts: Vec::new(),
                healthcheck: None,
                platform: None,
            });
            stub_id
        };

        self.edges.push(Edge {
            from: owner_id.to_string(),
            to: child_id.clone(),
            kind: "depends-on".into(),
            branch: default_branch.map(str::to_string),
            flows: Vec::new(),
        });

        Some(child_id)
    }

    /// Same-repo counterpart to `visit_service_dependency`: a `kind: service`
    /// dependency that omitted `repo` names a sibling service already
    /// declared in this same repo's own `services:` map — nothing to clone,
    /// no stub, just a `depends-on` edge (or a warning, same as
    /// `visit_local_service`, if `wanted_service` is ambiguous or missing).
    pub(crate) fn visit_sibling_service_dependency(
        &mut self,
        owner_id: &str,
        local_path: &str,
        wanted_service: Option<&str>,
    ) -> Option<String> {
        let component = self.scanned.get(local_path)?;
        let child_id = self.visit_local_service(local_path, component, wanted_service)?;
        if child_id == owner_id {
            self.warnings.push(Warning::blocking(format!(
                "'{owner_id}' declares a same-repo `kind: service` dependency on itself"
            )));
            return None;
        }
        self.edges.push(Edge {
            from: owner_id.to_string(),
            to: child_id.clone(),
            kind: "depends-on".into(),
            branch: None,
            flows: Vec::new(),
        });
        Some(child_id)
    }

    pub(crate) fn visit_dependency(&mut self, owner_id: &str, local_path: &str, dep: Dependency) {
        match dep {
            Dependency::Service {
                repo,
                default_branch,
                services,
            } => {
                // One dependency block can name several of the target repo's
                // services (see `#GitDependency.services`) — resolving to a
                // `local_path` (or, for the same-repo form below, reusing the
                // owner's own) happens once inside each call, and
                // `visit_local_services` is itself idempotent per repo (see
                // its `self.visited` guard), so repeating this per name costs
                // nothing extra beyond the one `depends-on` edge each needs.
                // No names given at all: same as before, depend on "the"
                // service (the sole one, or a warning if that's ambiguous).
                let wanted: Vec<Option<&str>> = if services.is_empty() {
                    vec![None]
                } else {
                    services.iter().map(|n| Some(n.as_str())).collect()
                };
                match repo {
                    Some(repo) => {
                        for name in wanted {
                            self.visit_service_dependency(
                                owner_id,
                                &repo,
                                default_branch.as_deref(),
                                name,
                            );
                        }
                    }
                    None => {
                        // Same repo, no `repo:` given — reference a sibling
                        // service already declared in this repo's own
                        // `services:` map instead of cloning anything.
                        for name in wanted {
                            self.visit_sibling_service_dependency(owner_id, local_path, name);
                        }
                    }
                }
            }
            Dependency::Backing(backing) => {
                let BackingDependencyConfig {
                    name,
                    image,
                    environment,
                    ports,
                    domain_scope,
                    command,
                    volumes,
                    platform,
                    env_file,
                    restart,
                    user,
                    working_dir,
                    labels,
                    cap_add,
                    cap_drop,
                    privileged,
                    extra_hosts,
                    healthcheck,
                } = *backing;
                // Leaf-first, same convention as service ids and named
                // ports (`{port_name}.{node's domain}`): the specific thing
                // comes first, its owning scope after.
                let backing_id = format!("{name}.{owner_id}");
                let ports = ports.into_map();
                self.check_port_config(&backing_id, &ports);
                // A backing dependency has no checkout of its own — it's
                // declared inline in the owning service's `.fghj.yaml` — but
                // it still belongs to that service's repo/branch, and its
                // dirty status *is* the owning checkout's, since editing the
                // dependency block is editing that same working tree. The
                // owner's `Node` is always already in `self.nodes` here:
                // it's inserted before the loop that calls `visit_dependency`
                // for any of its own dependencies.
                let owner = self.nodes.get(owner_id);
                let owner_repo = owner.and_then(|o| o.repo.clone());
                let owner_branch = owner.and_then(|o| o.branch.clone());
                let owner_dirty = owner.is_some_and(|o| o.dirty);
                self.nodes
                    .entry(backing_id.clone())
                    .or_insert_with(|| Node {
                        id: backing_id.clone(),
                        label: name.to_string(),
                        kind: "backing".into(),
                        image: Some(image),
                        branch: owner_branch,
                        repo: owner_repo,
                        domain_scope,
                        local_path: None,
                        domain: String::new(),
                        downloaded: true,
                        dirty: owner_dirty,
                        flows: Vec::new(),
                        build: None,
                        ports,
                        environment: environment.to_pairs(),
                        command,
                        volumes,
                        additional_hosts: Vec::new(),
                        wildcard_hosts: Vec::new(),
                        env_file,
                        restart,
                        user,
                        working_dir,
                        labels,
                        cap_add,
                        cap_drop,
                        privileged,
                        extra_hosts,
                        healthcheck,
                        platform,
                    });
                self.edges.push(Edge {
                    from: owner_id.to_string(),
                    to: backing_id,
                    kind: "owns".into(),
                    branch: None,
                    flows: Vec::new(),
                });
            }
            Dependency::SharedBacking {
                repo,
                service,
                name,
            } => {
                // References the owning service by `repo` (if given, like
                // `#GitDependency`) + `service` — a bare service name alone
                // isn't unique (two peer repos, or two services in the same
                // repo, can share one), so both are needed to identify one
                // unambiguous node. Omitting `repo` means "a sibling service
                // in this same repo" (`local_path`, passed in by the caller).
                // Resolved without registering a `depends-on` edge or
                // recursing into it (this is a reference to an
                // already-declared backing dependency, not a new one).
                let target_local_path = match &repo {
                    Some(repo) => {
                        let norm = normalize_repo_url(repo);
                        self.repo_index
                            .get(&norm)
                            .cloned()
                            .unwrap_or_else(|| repo_name_from_url(repo))
                    }
                    None => local_path.to_string(),
                };
                let target_owner_id = format!("{service}.{target_local_path}");
                let backing_id = format!("{name}.{target_owner_id}");
                self.edges.push(Edge {
                    from: owner_id.to_string(),
                    to: backing_id,
                    kind: "shared-backing".into(),
                    branch: None,
                    flows: Vec::new(),
                });
            }
        }
    }
}
