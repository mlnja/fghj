//! Resolving one declared dependency into nodes and edges — a git
//! dependency on another repo, a sibling service, or a backing dependency.

//! `ResolveCtx` — the traversal that walks components and their
//! dependencies, turning parsed config into graph nodes and edges.

use std::collections::BTreeMap;

use super::config::{default_domain_scope, default_restart, default_stop_grace_period};
use super::dependency::{BackingDependencyConfig, Dependency, TaskConfig};
use super::graph::{Edge, Node};
use super::repo_url::{normalize_repo_url, repo_name_from_url};

use super::visit::{PendingAfter, ResolveCtx};
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
            self.visit_local_service(&local_path, component, wanted_service, "services:")?
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
                head: None,
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
                stop_signal: None,
                stop_grace_period: default_stop_grace_period(),
                user: None,
                working_dir: None,
                labels: BTreeMap::new(),
                cap_add: Vec::new(),
                cap_drop: Vec::new(),
                privileged: false,
                extra_hosts: Vec::new(),
                healthcheck: None,
                platform: None,
                run_policy: None,
                // Unknown until the repo is actually pulled and its
                // `.fghj.yaml` read, same as `healthcheck`/`platform` above.
                debug: None,
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
        let child_id =
            self.visit_local_service(local_path, component, wanted_service, "services:")?;
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
                    stop_signal,
                    stop_grace_period,
                    user,
                    working_dir,
                    labels,
                    cap_add,
                    cap_drop,
                    privileged,
                    extra_hosts,
                    healthcheck,
                    debug,
                } = *backing;
                // Leaf-first, same convention as service ids and named
                // ports (`{port_name}.{node's domain}`): the specific thing
                // comes first, its owning scope after.
                let backing_id = format!("{name}.{owner_id}");
                let ports = ports.into_map();
                self.check_port_config(&backing_id, &ports);
                self.check_stop_signal(&backing_id, stop_signal.as_deref());
                self.check_healthcheck(&backing_id, healthcheck.as_ref());
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
                let owner_head = owner.and_then(|o| o.head.clone());
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
                        head: owner_head,
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
                        stop_signal,
                        stop_grace_period,
                        user,
                        working_dir,
                        labels,
                        cap_add,
                        cap_drop,
                        privileged,
                        extra_hosts,
                        healthcheck,
                        platform,
                        run_policy: None,
                        debug,
                    });
                self.edges.push(Edge {
                    from: owner_id.to_string(),
                    to: backing_id,
                    kind: "owns".into(),
                    branch: None,
                    flows: Vec::new(),
                });
            }
            Dependency::Task(task) => self.visit_task_dependency(owner_id, local_path, *task),
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

    /// Resolves a `#Task` — the terminating node kind — into a node plus the
    /// edges that order it.
    ///
    /// Two edges, and they mean different things. `owner -> task` is `owns`,
    /// the same relation a `#BackingDependency` gets: the task belongs to
    /// this service and the service does not start until it has exited 0.
    /// `task -> sibling` is `after`, purely ordering: a seed needs its
    /// database healthy first, and "before the owner" alone does not say
    /// that.
    ///
    /// `after` targets are resolved by the same leaf-first convention every
    /// other id uses, and *not* checked here — the sibling may not have been
    /// visited yet. `resolve_universe` validates them in one pass once every
    /// node exists, exactly as it already does for `shared-backing`.
    fn visit_task_dependency(&mut self, owner_id: &str, local_path: &str, task: TaskConfig) {
        let TaskConfig {
            name,
            image,
            command,
            environment,
            volumes,
            after,
            run,
            platform,
            env_file,
            user,
            working_dir,
            labels,
            cap_add,
            cap_drop,
            privileged,
            extra_hosts,
            stop_signal,
            stop_grace_period,
            debug,
        } = task;
        let task_id = format!("{name}.{owner_id}");
        self.check_stop_signal(&task_id, stop_signal.as_deref());

        // A task with nothing to run would re-run the image's own
        // long-running `CMD` and never exit, hanging the run until the
        // health budget expires. CUE rejects it too (`[_, ...]`), but the
        // Rust types are the enforcing boundary — see `resolver::name`'s
        // module doc on that split.
        if command.is_empty() {
            self.warnings.push(Warning::blocking(format!(
                "task '{task_id}' declares no `command` — a task is its command, and one without \
                 would run the image's default CMD and never exit"
            )));
        }

        // The owner is always already in `self.nodes`: it is inserted before
        // the loop that calls `visit_dependency` for any of its own
        // dependencies. Same reasoning as the backing arm above, plus one
        // more use — an image-less task runs the owner's own built image,
        // which is the common case (a migration is this service's code with
        // a different command).
        let owner = self.nodes.get(owner_id);
        let owner_repo = owner.and_then(|o| o.repo.clone());
        let owner_branch = owner.and_then(|o| o.branch.clone());
        let owner_dirty = owner.is_some_and(|o| o.dirty);
        let owner_head = owner.and_then(|o| o.head.clone());
        let inherited_build = owner.and_then(|o| o.build.clone());
        if image.is_none() && inherited_build.is_none() {
            self.warnings.push(Warning::blocking(format!(
                "task '{task_id}' declares no `image` and its owning service '{owner_id}' has no \
                 `build` to inherit one from"
            )));
        }

        self.nodes.entry(task_id.clone()).or_insert_with(|| Node {
            id: task_id.clone(),
            label: name.to_string(),
            kind: "task".into(),
            image,
            branch: owner_branch,
            repo: owner_repo,
            // A task is never routed to and never published — it has no
            // ports and answers on no domain. `domain_scope` still has to
            // say *something*, and "run" is the one that cannot collide
            // across runs.
            domain_scope: default_domain_scope(),
            // No checkout of its own; it lives in the owner's, same as a
            // backing dependency declared inline.
            local_path: None,
            domain: String::new(),
            downloaded: true,
            dirty: owner_dirty,
            head: owner_head,
            flows: Vec::new(),
            build: inherited_build,
            ports: BTreeMap::new(),
            environment: environment.to_pairs(),
            command,
            volumes,
            additional_hosts: Vec::new(),
            wildcard_hosts: Vec::new(),
            env_file,
            // Forced, not read from config: `TaskConfig` has no `restart`
            // field at all, because a restart policy on a container whose
            // purpose is to exit would restart it forever.
            restart: default_restart(),
            stop_signal,
            stop_grace_period,
            user,
            working_dir,
            labels,
            cap_add,
            cap_drop,
            privileged,
            extra_hosts,
            // Likewise absent from `TaskConfig`: an exited container can
            // never report Docker-`healthy`, which is the whole reason this
            // kind exists.
            healthcheck: None,
            platform,
            run_policy: Some(run),
            debug,
        });

        self.edges.push(Edge {
            from: owner_id.to_string(),
            to: task_id.clone(),
            kind: "owns".into(),
            branch: None,
            flows: Vec::new(),
        });

        for target in after {
            // Not resolved here: the sibling may not have been visited yet,
            // and which spelling is right depends on what kind of
            // dependency it turns out to be. `resolve_universe` resolves
            // and validates these in one pass once every node exists.
            self.pending_after.push(PendingAfter {
                task_id: task_id.clone(),
                owner_id: owner_id.to_string(),
                local_path: local_path.to_string(),
                target: target.to_string(),
            });
        }
    }
}
