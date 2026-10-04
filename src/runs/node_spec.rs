//! Derives the launch spec for a single node, including image resolution,
//! volume binds and environment assembly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::domain::{DomainZone, derive_domain};
use super::fqdn_template::expand_service_fqdn_templates;
use super::naming::derive_volume_name;
use super::spec::NodeSpec;
use crate::docker;
use crate::resolver::{Graph, Node, VolumeMount};
use crate::util::env_file::parse_env_file;
use crate::util::label::sanitize_label;

use super::registry::RunRegistry;

impl RunRegistry {
    /// Builds `tag` from `build`, rooted at `repo_root`, recording the
    /// attempt into `node_id`'s event stream so a failing build reads as a
    /// failing step rather than an opaque start error.
    ///
    /// Shared by the service arm of `resolve_node_spec` and its image-less
    /// task arm, which deliberately build the identical image under the
    /// identical tag — see that arm for why.
    async fn build_node_image(
        &self,
        run_id: &str,
        node_id: &str,
        repo_root: &Path,
        build: &crate::resolver::NodeBuild,
        tag: &str,
        platform: Option<&str>,
    ) -> Result<()> {
        let build_dir = repo_root.join(&build.context);
        self.record_event(
            run_id,
            node_id,
            "start",
            "building image",
            "running",
            Some(tag.to_string()),
        )
        .await;
        let secrets = match resolve_build_secrets(repo_root, &build.secrets) {
            Ok(secrets) => secrets,
            Err(e) => {
                self.record_event(
                    run_id,
                    node_id,
                    "start",
                    "building image",
                    "error",
                    Some(format!("{e:#}")),
                )
                .await;
                return Err(e);
            }
        };
        // Resolved here rather than passed in because the owner is ambient
        // daemon state (`fghj wire`), not a property of the run — and
        // re-derived on every build so a restarted agent is picked up, the
        // same reason `WorkspaceOwner::apply_to_command` doesn't trust the
        // stored path either.
        let ssh_auth_sock = if build.ssh {
            let owner = self.db.clone().load_owner().await.ok().flatten();
            match owner.and_then(|o| o.live_ssh_auth_sock()) {
                Some(sock) => Some(sock),
                None => {
                    let e = anyhow::anyhow!(
                        "build.ssh is set but no live ssh-agent was found for the workspace owner; \
                         run `fghj wire` from a shell with a running agent"
                    );
                    self.record_event(
                        run_id,
                        node_id,
                        "start",
                        "building image",
                        "error",
                        Some(format!("{e:#}")),
                    )
                    .await;
                    return Err(e);
                }
            }
        } else {
            None
        };
        let opts = docker::BuildOpts {
            context_dir: &build_dir,
            dockerfile: &build.dockerfile,
            tag,
            platform,
            args: &build.args,
            target: build.target.as_deref(),
            secrets: &secrets,
            ssh_auth_sock: ssh_auth_sock.as_deref(),
        };
        if let Err(e) = docker::build_image(&self.docker, &opts).await {
            self.record_event(
                run_id,
                node_id,
                "start",
                "building image",
                "error",
                Some(format!("{e:#}")),
            )
            .await;
            return Err(e);
        }
        self.record_event(run_id, node_id, "start", "building image", "ok", None)
            .await;
        Ok(())
    }

    /// The pure, side-effect-free half of resolving a node's config — image
    /// tag, env, port list, volume binds, domain aliases — shared between
    /// `start_node` (`side_effects: true`, which then actually builds the
    /// image / ensures named volumes exist / runs the container) and
    /// `config_drift` (`side_effects: false`, which only needs the
    /// same values to compute a comparable hash — see `spec_hash` — without
    /// building anything or touching Docker at all).
    pub(super) async fn resolve_node_spec(
        &self,
        graph: &Graph,
        node: &Node,
        run_id: &str,
        side_effects: bool,
    ) -> Result<Option<NodeSpec>> {
        let workspace = sanitize_label(&graph.workspace_name);
        let container_name = format!("fghj-{workspace}-{run_id}-{}", sanitize_label(&node.id));
        // Every node's domain is derived the same way, unconditionally —
        // there's no CUE-declared override for any node kind (services
        // included) that could bypass this, so two nodes can never collide
        // on a name the way a hand-written one could. Built from `node.id`
        // rather than `node.label`: `id` is a unique, leaf-first dotted
        // chain (`dep-name.owning-service-id` for backing deps — see
        // `resolver::visit_dependency` — or `service-name.repo-local-path`
        // for services — see `resolver::visit_local_service`), while `label`
        // is only the bare declared name and can collide, e.g. when two
        // peer repos each declare a same-named service, or two different
        // services each own their own same-named backing dependency.
        // `run_id` is folded in just like it is for
        // `container_name`/the network name above, *except* for the default
        // run: fghj models one shared, singular default environment per
        // workspace (see `ensure_running`), so it needs no disambiguating
        // segment — only a named/review run does, since more than one of
        // those can be alive at once. `node.domain_scope == "stable"` is the
        // other opt-out (CUE `#Service.domain_scope` /
        // `#BackingDependency.domain_scope`): a deliberate, explicit choice
        // by the CUE author to give a node one fixed identity shared across
        // every run, not just the default one.
        //
        // `domain` (the `fghj.internal` zone) is never registered as a
        // Docker alias on this node's own container — the run's sidecar
        // owns resolving it, universally, from inside this run's docker
        // network (see `dns.rs`'s module doc for the full zone split), so
        // it stays consistent with what the host's own DNS server answers
        // it with too. `raw_domain` (`fghj.raw.internal`) is the real
        // Docker network alias registered below: in-network-only, resolved
        // straight to this container's own IP by Docker's embedded DNS.
        let domain = derive_domain(
            &node.id,
            &node.domain_scope,
            &graph.workspace_name,
            run_id,
            DomainZone::Http,
        );
        let raw_domain = derive_domain(
            &node.id,
            &node.domain_scope,
            &graph.workspace_name,
            run_id,
            DomainZone::Raw,
        );

        // Where a node's relative bind-mount `host` / `env_file` paths
        // resolve against — the repo's checkout root, not `build.context`
        // (Compose resolves both relative to the compose file's directory;
        // this is the fghj equivalent). For a service, its own checkout
        // root; for a backing dependency, which has no checkout of its own,
        // the *owning* service's checkout root (set below, via the graph's
        // "owns" edge).
        let mut volume_base: Option<PathBuf> = None;

        let image = match node.kind.as_str() {
            "backing" => {
                // A backing dependency has no checkout of its own to resolve
                // a relative `env_file` (or bind-mount `host`) path against —
                // same rule Compose uses, resolving `env_file` against the
                // compose file's own directory regardless of `build` vs
                // `image`. Its equivalent of "the compose file's directory"
                // is the *owning* service's checkout root: the service whose
                // .fghj.yaml declares this dependency inline, found via the
                // graph's "owns" edge (`resolver::visit_dependency` always
                // pushes owner -> backing).
                let owner_local_path = graph
                    .edges
                    .iter()
                    .find(|e| e.kind == "owns" && e.to == node.id)
                    .and_then(|e| graph.nodes.iter().find(|n| n.id == e.from))
                    .and_then(|n| n.local_path.as_ref());
                if let Some(owner_local_path) = owner_local_path {
                    volume_base = Some(self.workspace.join(owner_local_path));
                }
                match node.image.clone() {
                    Some(img) => img,
                    None => bail!("backing node {} has no image", node.id),
                }
            }
            "task" => {
                // Same "no checkout of its own" situation as a backing
                // dependency, and resolved the same way: through the graph's
                // `owns` edge back to the service that declared it inline.
                let owner = graph
                    .edges
                    .iter()
                    .find(|e| e.kind == "owns" && e.to == node.id)
                    .and_then(|e| graph.nodes.iter().find(|n| n.id == e.from));
                if let Some(local_path) = owner.and_then(|o| o.local_path.as_ref()) {
                    volume_base = Some(self.workspace.join(local_path));
                }
                match node.image.clone() {
                    Some(img) => img,
                    None => {
                        // The common case: a migration is the owning
                        // service's own code run with a different command,
                        // so it runs the owning service's image — built
                        // under the *owner's* tag rather than one of its
                        // own. The two builds are identical by construction
                        // (the task inherited `build` wholesale from the
                        // owner — see `resolver::visit_task_dependency`), and
                        // a task starts *before* its owner, so a separate
                        // tag would mean building the same image twice per
                        // run under two names.
                        let Some(owner) = owner else {
                            bail!(
                                "task node {} has no owning service to inherit an image from",
                                node.id
                            );
                        };
                        let (Some(build), Some(local_path)) =
                            (node.build.clone(), owner.local_path.clone())
                        else {
                            bail!(
                                "task node {} declares no image and {} has no build to inherit",
                                node.id,
                                owner.id
                            );
                        };
                        let branch = owner.branch.clone().unwrap_or_else(|| "local".to_string());
                        let tag = format!(
                            "fghj/{}:{}",
                            sanitize_label(&owner.id),
                            sanitize_label(&branch)
                        );
                        let repo_root = self.workspace.join(&local_path);
                        if side_effects {
                            self.build_node_image(
                                run_id,
                                &node.id,
                                &repo_root,
                                &build,
                                &tag,
                                node.platform.as_deref(),
                            )
                            .await?;
                        }
                        tag
                    }
                }
            }
            _ => {
                let build = node.build.clone().unwrap_or(crate::resolver::NodeBuild {
                    context: ".".to_string(),
                    dockerfile: "Dockerfile".to_string(),
                    args: BTreeMap::new(),
                    target: None,
                    ssh: false,
                    secrets: Vec::new(),
                });

                let local_path = match node.local_path.clone() {
                    Some(p) => p,
                    None => bail!("service node {} has no local_path", node.id),
                };
                let branch = node.branch.clone().unwrap_or_else(|| "local".to_string());
                let tag = format!(
                    "fghj/{}:{}",
                    sanitize_label(&node.id),
                    sanitize_label(&branch)
                );
                let repo_root = self.workspace.join(&local_path);
                volume_base = Some(repo_root.clone());
                if side_effects {
                    self.build_node_image(
                        run_id,
                        &node.id,
                        &repo_root,
                        &build,
                        &tag,
                        node.platform.as_deref(),
                    )
                    .await?;
                }
                tag
            }
        };

        // Named ports (`#Port.name`) get their own domain, nested under this
        // node's raw domain — `admin.api.default.shop.fghj.raw.internal` —
        // and need to be real Docker aliases too, so a sibling container can
        // reach a specific named port directly by name instead of having to
        // know its container-side port number ahead of time.
        let mut aliases = vec![raw_domain.clone()];
        aliases.extend(
            node.ports
                .values()
                .filter_map(|p| p.name.as_ref())
                .map(|name| format!("{name}.{raw_domain}")),
        );

        let mut port_list: Vec<(String, Option<u16>)> = node
            .ports
            .iter()
            .map(|(port, cfg)| (port.clone(), cfg.host_port))
            .collect();
        // `debug` is published exactly like a declared port, which is the
        // whole mechanism: `raw_net::reconcile` NATs every *published* port
        // of a node to `{virtual_ip}:{container_port}` (see
        // `state::query::raw_endpoints`, which reads the full observed port
        // map rather than only the routed ones), so the debugger answers at
        // `{raw_domain}:{debug}` — on the declared number, from the host and
        // from inside the run's network alike, and without colliding with
        // the same port on any other node or any parallel run.
        //
        // Published unconditionally, even with nothing listening on the
        // other side: that keeps the address stable and printable before
        // anyone attaches, and means enabling `FGHJ_DEBUG_WAIT` changes only
        // the environment, never the port set.
        //
        // Skipped when the same number is already in `ports`, where an
        // explicit `#Port` may carry a `host_port` or a `name` this would
        // otherwise clobber — Docker would also reject the duplicate
        // binding.
        if let Some(debug_port) = node.debug {
            let key = debug_port.to_string();
            if !node.ports.contains_key(&key) {
                port_list.push((key, None));
            }
        }

        // A named volume's Docker-side existence is otherwise implicit (the
        // daemon auto-creates one, unlabeled, the first time a bind
        // references it) — `ensure_volume` here labels it so
        // `docker::remove_run_scoped_volumes` can find it in `stop()`.
        // `Iterator::map` can't `.await`, hence the explicit loop.
        let mut binds: Vec<String> = Vec::with_capacity(node.volumes.len());
        for v in &node.volumes {
            match v {
                VolumeMount::Bind {
                    host,
                    container,
                    read_only,
                } => {
                    let host_path = if Path::new(host).is_absolute() {
                        PathBuf::from(host)
                    } else {
                        volume_base
                            .as_ref()
                            .expect("node with volumes has a resolved checkout root")
                            .join(host)
                    };
                    // Resolve symlinks (notably macOS's `/var` -> `/private/var`)
                    // before handing the path to Docker: bind-mounting a file
                    // through a symlinked parent directory makes some Docker
                    // backends (OrbStack) misdetect the mount source's type and
                    // reject an otherwise-valid file-to-file bind mount.
                    let host_path = std::fs::canonicalize(&host_path).unwrap_or(host_path);
                    binds.push(format!(
                        "{}:{container}{}",
                        host_path.display(),
                        if *read_only { ":ro" } else { "" }
                    ));
                }
                VolumeMount::Named {
                    name,
                    scope,
                    container,
                    read_only,
                    shared,
                } => {
                    // `shared` drops the node-id qualification, which is the
                    // only way two nodes can land on one volume — see
                    // `derive_volume_name` for why that's opt-in.
                    let owner = (!shared).then_some(node.id.as_str());
                    let volume_name =
                        derive_volume_name(name, scope, owner, &graph.workspace_name, run_id);
                    if side_effects {
                        docker::ensure_volume(
                            &self.docker,
                            &volume_name,
                            &graph.workspace_name,
                            scope,
                            run_id,
                        )
                        .await?;
                    }
                    binds.push(format!(
                        "{volume_name}:{container}{}",
                        if *read_only { ":ro" } else { "" }
                    ));
                }
            }
        }

        // `env_file` entries load first, in declared order, then
        // `environment` is applied on top — same precedence as Compose,
        // relying on Docker's own last-value-wins behavior for a flat `-e`
        // list rather than de-duping keys here. Relative paths resolve
        // against `volume_base` — this node's own checkout root for a
        // service, or the owning service's for a backing dependency.
        let mut env: Vec<String> = Vec::new();
        for path in &node.env_file {
            let file_path = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                volume_base
                    .as_ref()
                    .expect("node with env_file has a resolved checkout root")
                    .join(path)
            };
            let contents = std::fs::read_to_string(&file_path)
                .with_context(|| format!("failed to read env_file {}", file_path.display()))?;
            env.extend(parse_env_file(&contents));
        }
        env.extend(node.environment.iter().cloned());
        for entry in &mut env {
            *entry =
                expand_service_fqdn_templates(entry, node, &raw_domain, &domain, graph, run_id);
        }

        // Nothing is appended here for `node.debug`, and that absence is
        // deliberate. `debug` is a port declaration, exactly like `ports`,
        // and fghj injects nothing into the environment for those either:
        // the app already knows which port it listens on, and the config
        // declares it so fghj can publish and address it. A
        // `FGHJ_DEBUG_PORT` would just be the same number a second time, in
        // a second place, able to disagree with the first.
        //
        // The one thing an image genuinely cannot know on its own is whether
        // the operator has asked *this container* to halt at startup — that
        // is `FGHJ_DEBUG_WAIT`, and it is set in `start_node`, after
        // `spec_hash`, because it is runtime state rather than config. See
        // `state::ContainerDesired::debug_wait`.

        Ok(Some(NodeSpec {
            container_name,
            domain,
            raw_domain,
            aliases,
            image,
            port_list,
            binds,
            env,
        }))
    }
}

/// Turns `#Build.secrets` into the `(id, absolute path)` pairs
/// `docker::BuildOpts` wants, resolving each `file` against the repo checkout
/// root exactly like a bind mount's `host` side.
///
/// A missing file is an error rather than an empty secret, because the
/// failure it would otherwise produce happens *inside* the Dockerfile — a
/// `RUN` that reads an empty `/run/secrets/npmrc` and fails with a 401 from
/// some registry, which is a long way from "you forgot to create the file".
fn resolve_build_secrets(
    repo_root: &Path,
    secrets: &[crate::resolver::NodeBuildSecret],
) -> Result<Vec<(String, std::path::PathBuf)>> {
    secrets
        .iter()
        .map(|secret| {
            let path = repo_root.join(&secret.file);
            if !path.is_file() {
                bail!(
                    "build secret {} points at {}, which is not a file",
                    secret.id,
                    path.display()
                );
            }
            Ok((secret.id.clone(), path))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::persistence::WorkspaceDb;
    use crate::resolver::NodeBuildSecret;
    use crate::resolver::port::PortConfig;
    use crate::runs::testing::{test_graph, test_node};

    fn secret(id: &str, file: &str) -> NodeBuildSecret {
        NodeBuildSecret {
            id: id.to_string(),
            file: file.to_string(),
        }
    }

    #[test]
    fn a_secret_file_is_resolved_against_the_repo_checkout_root() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("ci")).unwrap();
        std::fs::write(repo.path().join("ci/npmrc"), "token").unwrap();

        let resolved = resolve_build_secrets(repo.path(), &[secret("npmrc", "ci/npmrc")]).unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "npmrc");
        assert_eq!(resolved[0].1, repo.path().join("ci/npmrc"));
    }

    /// Fails before the build rather than during it — see
    /// `resolve_build_secrets` for why a missing file can't be treated as an
    /// empty secret. The message has to name the path, since "which file did
    /// it look for" is the entire question the user has at that moment.
    #[test]
    fn a_missing_secret_file_fails_the_build_up_front() {
        let repo = tempfile::tempdir().unwrap();

        let err = resolve_build_secrets(repo.path(), &[secret("npmrc", "ci/npmrc")])
            .expect_err("a secret pointing at nothing must not build");

        let message = format!("{err:#}");
        assert!(message.contains("npmrc"), "got: {message}");
        assert!(message.contains("ci/npmrc"), "got: {message}");
    }

    /// A directory is the shape of mistake that would otherwise reach
    /// BuildKit and fail there instead.
    #[test]
    fn a_secret_pointing_at_a_directory_is_rejected_too() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("ci")).unwrap();

        assert!(resolve_build_secrets(repo.path(), &[secret("npmrc", "ci")]).is_err());
    }

    /// A `RunRegistry` whose Docker handle is never dialled — see
    /// `docker::undialled_client`. Every test below resolves a node that
    /// declares an `image:` with `side_effects: false`, which is the wholly
    /// pure path: no build, no `ensure_volume`, no daemon.
    fn registry() -> (RunRegistry, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        let registry = RunRegistry::new(
            tmp.path().to_path_buf(),
            db,
            Arc::new(crate::docker::undialled_client()),
        );
        (registry, tmp)
    }

    /// A backing node, which needs nothing from the workspace on disk.
    fn debuggable(debug: Option<u16>) -> crate::resolver::Node {
        let mut node = test_node("api", "api", "backing");
        node.image = Some("busybox".to_string());
        node.debug = debug;
        node
    }

    async fn spec_for(node: &crate::resolver::Node) -> NodeSpec {
        let (registry, _tmp) = registry();
        let graph = test_graph(vec![node.clone()], Vec::new());
        registry
            .resolve_node_spec(&graph, node, "default", false)
            .await
            .expect("resolving an image-backed node is pure")
            .expect("side_effects: false still yields a spec")
    }

    /// The whole of `debug:`, in one assertion: the port joins the published
    /// set, and nothing is injected into the environment.
    ///
    /// `debug` is a port declaration like `ports`, so it behaves like one.
    /// An earlier draft also set `FGHJ_DEBUG=1` and `FGHJ_DEBUG_PORT`; both
    /// were dropped because the image already knows which port it listens
    /// on — the number would just be restated in a second place, free to
    /// disagree with the first.
    #[tokio::test]
    async fn declaring_debug_publishes_the_port_and_injects_no_environment() {
        let spec = spec_for(&debuggable(Some(9229))).await;

        assert_eq!(spec.port_list, vec![("9229".to_string(), None)]);
        assert!(spec.env.is_empty(), "got: {:?}", spec.env);
    }

    /// `FGHJ_DEBUG_WAIT` — the only variable fghj sets at all, and the only
    /// thing an image genuinely cannot work out for itself — is emphatically
    /// not in the *spec*: `start_node` appends it after `spec_hash`, so a
    /// halted container never reads as drifted. See
    /// `state::ContainerDesired::debug_wait`.
    #[tokio::test]
    async fn the_wait_variable_is_never_part_of_the_spec() {
        let spec = spec_for(&debuggable(Some(5678))).await;
        assert!(!spec.env.iter().any(|e| e.starts_with("FGHJ_DEBUG")));
    }

    #[tokio::test]
    async fn a_node_without_debug_publishes_nothing() {
        let spec = spec_for(&debuggable(None)).await;

        assert!(spec.port_list.is_empty());
        assert!(spec.env.is_empty());
    }

    /// The same number declared both ways must publish once. Docker rejects
    /// a duplicate binding outright, and the explicit `#Port` is the one
    /// that carries a pinned `host_port` and a `name`, so it wins.
    #[tokio::test]
    async fn a_debug_port_already_declared_in_ports_is_not_published_twice() {
        let mut node = debuggable(Some(9229));
        node.ports.insert(
            "9229".to_string(),
            PortConfig {
                host_port: Some(19229),
                name: Some(crate::resolver::name::Name::parse("inspector").unwrap()),
                ..Default::default()
            },
        );

        let spec = spec_for(&node).await;

        assert_eq!(spec.port_list, vec![("9229".to_string(), Some(19229))]);
        // ...and the named-port alias the explicit declaration asked for is
        // still registered, which is what would have been lost.
        assert!(spec.aliases.iter().any(|a| a.starts_with("inspector.")));
    }

    /// `debug` has to reach `spec_hash`, and it does so purely through
    /// `port_list`: adding it to `.fghj.yaml` changes the container's
    /// published ports, which is genuine drift. This is the assertion that
    /// keeps that true now that no environment variable carries it.
    #[tokio::test]
    async fn adding_debug_to_the_config_reads_as_drift() {
        let plain = debuggable(None);
        let debugging = debuggable(Some(9229));

        let before = crate::runs::spec::spec_hash(&plain, &spec_for(&plain).await);
        let after = crate::runs::spec::spec_hash(&debugging, &spec_for(&debugging).await);

        assert_ne!(before, after);
    }
}
