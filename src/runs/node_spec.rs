//! Derives the launch spec for a single node, including image resolution,
//! volume binds and environment assembly.

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

/// Build-context size for the events pane. Binary units, one decimal, since
/// the number is read to answer "is that bigger than I expected" and not to
/// be added up.
fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KIB {
        return format!("{bytes} B context");
    }
    for (limit, unit) in [(KIB * KIB, "KiB"), (KIB * KIB * KIB, "MiB")] {
        if b < limit {
            return format!("{:.1} {unit} context", b / (limit / KIB));
        }
    }
    format!("{:.1} GiB context", b / (KIB * KIB * KIB))
}

/// Build duration, at the precision anyone actually reads it at: a build is
/// either seconds or minutes, and tenths stop mattering past ten seconds.
fn human_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 10.0 {
        return format!("{secs:.1}s");
    }
    if secs < 60.0 {
        return format!("{:.0}s", secs);
    }
    format!("{}m {:02}s", (secs / 60.0) as u64, (secs % 60.0) as u64)
}

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
        // A git build context builds from its clone in `.fghj/sources/`,
        // which pull makes and start never does — see
        // `concepts/git-build-sources.md`.
        let build_dir = match &build.source {
            Some(source) if !source.downloaded => {
                let e = anyhow::anyhow!(
                    "source not pulled: {} is built from {}, which isn't cloned into {} yet; \
                     pull this node, its flow or the whole workspace first",
                    node_id,
                    source.url,
                    source.path
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
            Some(source) => self.workspace.join(&source.path).join(&build.context),
            None => repo_root.join(&build.context),
        };
        let dockerfile = match &build.dockerfile_inline {
            Some(_) => docker::INLINE_DOCKERFILE,
            None => build.dockerfile.as_str(),
        };
        let owner = self.db.clone().load_owner().await.ok().flatten();
        // Where `docker build` would have built this, so fghj builds there
        // too. Read from the *owner's* home, never the process's: `fghjd` is
        // root, and `/var/root/.docker` holds no buildx config, which would
        // silently resolve to "use the embedded builder" — the exact wrong
        // answer. `None` keeps today's behaviour.
        let builder = owner
            .as_ref()
            .and_then(|o| crate::buildx::default_builder(std::path::Path::new(&o.home)));
        // What is being built, not just what it will be called. A node whose
        // `context`/`dockerfile`/`target` resolved to something other than
        // what its author expected looks identical in the events pane to one
        // that didn't, and the tag alone can't tell them apart — the tag is
        // derived from the node id, so it is the same either way.
        let mut what = format!("{tag}  ·  {}", build_dir.display());
        if let Some(source) = &build.source {
            what.push_str(&format!("  ·  from {}", source.url));
        }
        if build.dockerfile_inline.is_some() {
            what.push_str("  ·  inline Dockerfile");
        } else if build.dockerfile != "Dockerfile" {
            what.push_str(&format!("  ·  -f {}", build.dockerfile));
        }
        if let Some(target) = build.target.as_deref() {
            what.push_str(&format!("  ·  --target {target}"));
        }
        if let Some(platform) = platform {
            what.push_str(&format!("  ·  {platform}"));
        }
        // Which BuildKit, because there are two on a typical machine and they
        // are not interchangeable: a Dockerfile that builds under `docker
        // build` can fail here purely because dockerd's embedded BuildKit is
        // several versions behind the builder buildx selected. `None` means
        // the CLI uses the embedded one too, so there is nothing to disagree
        // with.
        what.push_str(&match &builder {
            Some(b) => format!("  ·  buildkit: {}", b.builder),
            None => String::from("  ·  buildkit: dockerd"),
        });
        self.record_event(
            run_id,
            node_id,
            "start",
            "building image",
            "running",
            Some(what),
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
            match owner.as_ref().and_then(|o| o.live_ssh_auth_sock()) {
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
            dockerfile,
            dockerfile_inline: build.dockerfile_inline.as_deref(),
            skip_git_dir: build.source.is_some(),
            tag,
            platform,
            args: &build.args,
            target: build.target.as_deref(),
            secrets: &secrets,
            ssh_auth_sock: ssh_auth_sock.as_deref(),
            builder: builder.as_ref(),
        };
        let started = std::time::Instant::now();
        let report = match docker::build_image(&self.docker, &opts).await {
            Ok(report) => report,
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
        // A build that succeeded still has two numbers worth seeing: how long
        // it took, and how much context was shipped to get there. The second
        // is the one nothing else would ever surface — the context is tarred
        // and uploaded whole on every build, so a `node_modules` or a `.git`
        // that should have been in `.dockerignore` shows up here as a cost
        // paid on every single start, and nowhere else.
        self.record_event(
            run_id,
            node_id,
            "start",
            "building image",
            "ok",
            Some(format!(
                "{} in {}",
                human_bytes(report.context_bytes),
                human_duration(started.elapsed())
            )),
        )
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
        // There is one environment per workspace, so nothing else needs
        // folding in.
        //
        // `domain` (the `fghj.internal` zone) is never registered as a
        // Docker alias on this node's own container — the run's sidecar
        // owns resolving it, universally, from inside this run's docker
        // network (see `dns.rs`'s module doc for the full zone split), so
        // it stays consistent with what the host's own DNS server answers
        // it with too. `raw_domain` (`fghj.raw.internal`) is the real
        // Docker network alias registered below: in-network-only, resolved
        // straight to this container's own IP by Docker's embedded DNS.
        let domain = derive_domain(&node.id, &graph.workspace_name, DomainZone::Http);
        let raw_domain = derive_domain(&node.id, &graph.workspace_name, DomainZone::Raw);

        // Where a node's relative bind-mount `host` / `env_file` paths
        // resolve against — the checkout root of the repo that declares the
        // node, not `build.context` (Compose resolves both relative to the
        // compose file's directory; this is the fghj equivalent). Every
        // node, backing services and tasks included, belongs to a repo.
        let repo_root = node.local_path.as_ref().map(|p| self.workspace.join(p));
        let volume_base = repo_root.clone();

        // `build` means fghj builds it, from this node's own repo, under a
        // tag of its own; otherwise it runs the published `image`. The
        // resolver refuses a node with both or neither. A task that is the
        // repo's own code run with another command declares the same
        // `build` and gets its own tag — Docker's layer cache makes the
        // second build of identical inputs a no-op.
        let image = match (&node.build, &node.image) {
            (Some(build), _) => {
                let Some(repo_root) = &repo_root else {
                    bail!(
                        "node {} has a build but no local_path to build from",
                        node.id
                    );
                };
                // Named after what the code is: the source's ref for a git
                // build context, otherwise the repo's branch.
                let branch = match &build.source {
                    Some(source) => source
                        .reference
                        .clone()
                        .unwrap_or_else(|| "default".to_string()),
                    None => node.branch.clone().unwrap_or_else(|| "local".to_string()),
                };
                let tag = format!(
                    "fghj/{}:{}",
                    sanitize_label(&node.id),
                    sanitize_label(&branch)
                );
                if side_effects {
                    self.build_node_image(
                        run_id,
                        &node.id,
                        repo_root,
                        build,
                        &tag,
                        node.platform.as_deref(),
                    )
                    .await?;
                }
                tag
            }
            (None, Some(image)) => image.clone(),
            (None, None) => bail!("node {} has neither a build nor an image", node.id),
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
        // Every container gets fghj's trust files, read-only, before its own
        // volumes. The zone's CA is useless to a service that cannot verify
        // it, and the alternative was each workspace writing out the same
        // `volumes:` entry by hand — naming a host path (`/private/var/...`
        // on macOS, `/var/...` elsewhere) that only fghj can actually know.
        // A host path nobody has to type is a host path nobody can get wrong.
        //
        // No `canonicalize` here, unlike the author-written binds below:
        // `fghjd_root()` is already resolved symlink-free at the source.
        let mut binds: Vec<String> = Vec::with_capacity(node.volumes.len() + 1);
        binds.push(format!(
            "{}:{}:ro",
            crate::daemon::certs_dir().display(),
            crate::daemon::CERTS_MOUNT
        ));
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
                    container,
                    read_only,
                    shared,
                } => {
                    // `shared` drops the node-id qualification, which is the
                    // only way two nodes can land on one volume — see
                    // `derive_volume_name` for why that's opt-in.
                    let owner = (!shared).then_some(node.id.as_str());
                    let volume_name = derive_volume_name(name, owner, &graph.workspace_name);
                    if side_effects {
                        docker::ensure_volume(&self.docker, &volume_name, &graph.workspace_name)
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
            *entry = expand_service_fqdn_templates(entry, node, &raw_domain, &domain, graph);
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

    /// The context size is the number in this pair that nobody is expecting,
    /// so it has to be unambiguous at a glance — a stray factor of 1024 here
    /// would read as a `.dockerignore` problem that isn't one, or hide one
    /// that is.
    #[test]
    fn a_context_size_reads_in_the_unit_a_human_would_pick() {
        assert_eq!(human_bytes(0), "0 B context");
        assert_eq!(human_bytes(900), "900 B context");
        // Exactly at a boundary belongs to the larger unit, not "1024.0 B".
        assert_eq!(human_bytes(1024), "1.0 KiB context");
        assert_eq!(human_bytes(1536), "1.5 KiB context");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB context");
        // The case this exists for: a `node_modules` nobody ignored.
        assert_eq!(human_bytes(412 * 1024 * 1024), "412.0 MiB context");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB context");
    }

    #[test]
    fn a_build_duration_drops_its_tenths_once_they_stop_mattering() {
        use std::time::Duration;
        assert_eq!(human_duration(Duration::from_millis(1340)), "1.3s");
        assert_eq!(human_duration(Duration::from_millis(9949)), "9.9s");
        // Past ten seconds the tenth is noise, and past a minute the minutes
        // are what the reader is counting.
        assert_eq!(human_duration(Duration::from_millis(10_400)), "10s");
        assert_eq!(human_duration(Duration::from_secs(59)), "59s");
        assert_eq!(human_duration(Duration::from_secs(60)), "1m 00s");
        // The seconds stay two digits so a column of these lines up.
        assert_eq!(human_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(human_duration(Duration::from_secs(8 * 60 + 7)), "8m 07s");
    }

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

    /// A node that declares no volumes at all still gets one: fghj's trust
    /// files, read-only. This is the assertion that keeps the feature from
    /// quietly becoming opt-in again, and the `:ro` from quietly dropping
    /// off — the directory it points at is the daemon's, and a container with
    /// write access to it could hand every other container on the machine a
    /// CA of its choosing.
    #[tokio::test]
    async fn every_container_mounts_the_trust_files_read_only() {
        let node = debuggable(None);
        assert!(
            node.volumes.is_empty(),
            "the point is a node that asks for nothing"
        );

        let spec = spec_for(&node).await;

        let expected = format!(
            "{}:{}:ro",
            crate::daemon::certs_dir().display(),
            crate::daemon::CERTS_MOUNT
        );
        assert_eq!(spec.binds, vec![expected]);
    }

    /// An author's own volumes come *after* fghj's, so a workspace that
    /// genuinely wants something else at `/etc/fghj/certs` can still put it
    /// there — Docker takes the last mount at a path. Nothing in fghj needs
    /// that, but a mount fghj forces and an author cannot override is a
    /// corner nobody can get out of.
    #[tokio::test]
    async fn an_authors_own_volumes_come_after_fghjs() {
        let mut node = debuggable(None);
        node.volumes.push(crate::resolver::VolumeMount::Named {
            name: crate::resolver::name::Name::parse("data").unwrap(),
            container: "/var/lib/data".to_string(),
            read_only: false,
            shared: false,
        });

        let spec = spec_for(&node).await;

        assert_eq!(spec.binds.len(), 2);
        assert!(spec.binds[0].ends_with(":ro"));
        assert!(spec.binds[1].ends_with(":/var/lib/data"));
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
