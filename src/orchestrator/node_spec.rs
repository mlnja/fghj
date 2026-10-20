use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::domain::{DomainZone, derive_domain};
use super::fqdn_template::expand_service_fqdn_templates;
use super::naming::{container_name, derive_volume_name};
use super::spec::NodeSpec;
use crate::docker;
use crate::resolver::{Graph, Node, VolumeMount};
use crate::util::env_file::parse_env_file;
use crate::util::label::sanitize_label;

use super::registry::Orchestrator;

const LARGE_CONTEXT: u64 = 1024 * 1024 * 1024;

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

impl Orchestrator {
    async fn build_node_image(
        &self,
        node_id: &str,
        repo_root: &Path,
        build: &crate::resolver::NodeBuild,
        tag: &str,
        platform: Option<&str>,
    ) -> Result<()> {
        let ev = self.events(node_id, "start");
        let started = std::time::Instant::now();
        let built: Result<docker::BuildReport> = async {
            // A git build context builds from its clone, which only pull creates.
            let build_dir = match &build.source {
                Some(source) if !source.downloaded => bail!(
                    "source not pulled: {} is built from {}, which isn't cloned into {} yet; \
                     pull this node, its flow or the whole workspace first",
                    node_id,
                    source.url,
                    source.path
                ),
                Some(source) => self.workspace.join(&source.path).join(&build.context),
                None => repo_root.join(&build.context),
            };
            let dockerfile = match &build.dockerfile_inline {
                Some(_) => docker::INLINE_DOCKERFILE,
                None => build.dockerfile.as_str(),
            };
            let owner = crate::persistence::workspace_owner(self.db.clone(), &self.workspace).await;
            // Read from the owner's home: `fghjd` is root, whose buildx config is empty.
            let builder = owner
                .as_ref()
                .and_then(|o| crate::buildx::default_builder(std::path::Path::new(&o.home)));
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
            what.push_str(&match &builder {
                Some(b) => format!("  ·  buildkit: {}", b.builder),
                None => String::from("  ·  buildkit: dockerd"),
            });
            ev.record("building image", "running", Some(what.clone()))
                .await;
            let home = owner.as_ref().map(|o| o.home.as_str());
            let secrets = resolve_build_secrets(repo_root, home, &build.secrets)?;
            // Re-derived on every build so a restarted agent is picked up.
            let ssh_auth_sock = if build.ssh {
                Some(owner.as_ref().and_then(|o| o.live_ssh_auth_sock()).context(
                    "build.ssh is set but no live ssh-agent was found for the workspace owner; \
                     run `fghj wire` from a shell with a running agent",
                )?)
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
            let context = docker::build_context(&opts).await?;
            what.push_str(&format!(
                "  ·  sending {}",
                human_bytes(context.len() as u64)
            ));
            // Held in memory and sent whole on every build; past this, it's
            // nearly always something `.dockerignore` should leave out.
            if context.len() as u64 > LARGE_CONTEXT {
                what.push_str(&format!(
                    ". That's large: whatever in {} the Dockerfile doesn't COPY \
                     belongs in its .dockerignore",
                    build_dir.display()
                ));
            }
            ev.record("building image", "running", Some(what)).await;
            docker::build_image_from(&self.docker, &opts, context).await
        }
        .await;
        match built {
            Ok(report) => {
                let took = format!(
                    "{} in {}",
                    human_bytes(report.context_bytes),
                    human_duration(started.elapsed())
                );
                ev.record("building image", "ok", Some(took)).await;
                Ok(())
            }
            Err(e) => {
                ev.failed("building image", &e).await;
                Err(e)
            }
        }
    }

    /// `side_effects: false` only computes the spec (for drift checks): no build,
    /// no volume creation.
    pub(super) async fn resolve_node_spec(
        &self,
        graph: &Graph,
        node: &Node,
        side_effects: bool,
    ) -> Result<NodeSpec> {
        let container_name = container_name(&graph.workspace_name, &node.id);
        // `domain` is resolved by the sidecar, not a Docker alias; `raw_domain` is a
        // real alias, resolved by Docker's embedded DNS.
        let domain = derive_domain(&node.id, &graph.workspace_name, DomainZone::Http);
        let raw_domain = derive_domain(&node.id, &graph.workspace_name, DomainZone::Raw);

        // Relative `host` / `env_file` paths resolve against the declaring repo's root.
        let repo_root = node.local_path.as_ref().map(|p| self.workspace.join(p));
        // `~` is the workspace owner's home: `fghjd` runs as root.
        let home = crate::persistence::workspace_owner(self.db.clone(), &self.workspace)
            .await
            .map(|o| o.home);

        let image = match (&node.build, &node.image) {
            (Some(build), _) => {
                let Some(repo_root) = &repo_root else {
                    bail!(
                        "node {} has a build but no local_path to build from",
                        node.id
                    );
                };
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
        // `debug` is published like a declared port, so raw_net exposes it at
        // `{raw_domain}:{debug}`. Skipped if `ports` already declares it.
        if let Some(debug_port) = node.debug {
            let key = debug_port.to_string();
            if !node.ports.contains_key(&key) {
                port_list.push((key, None));
            }
        }

        // fghj's trust files go first, read-only; an author's own mount at the same
        // path wins. `fghjd_root()` is already symlink-free.
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
                    let host_path = host_path(
                        host,
                        repo_root
                            .as_deref()
                            .expect("node with volumes has a resolved checkout root"),
                        home.as_deref(),
                    )?;
                    // Resolve symlinks (macOS `/var`): OrbStack rejects file binds through one.
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

        // `env_file` first, then `environment` on top; Docker keeps the last value.
        let mut env: Vec<String> = Vec::new();
        for path in &node.env_file {
            let file_path = host_path(
                path,
                repo_root
                    .as_deref()
                    .expect("node with env_file has a resolved checkout root"),
                home.as_deref(),
            )?;
            let contents = std::fs::read_to_string(&file_path)
                .with_context(|| format!("failed to read env_file {}", file_path.display()))?;
            env.extend(parse_env_file(&contents));
        }
        env.extend(node.environment.iter().cloned());
        for entry in &mut env {
            *entry = expand_service_fqdn_templates(entry, node, &raw_domain, &domain, graph);
        }

        Ok(NodeSpec {
            container_name,
            domain,
            raw_domain,
            aliases,
            image,
            port_list,
            binds,
            env,
        })
    }
}

/// A path on the host as written in `.fghj.yaml`: `~` or `~/…` is under
/// `home`, a relative one is under the repo, an absolute one is itself.
fn host_path(raw: &str, repo_root: &Path, home: Option<&str>) -> Result<PathBuf> {
    let rest = match raw.strip_prefix('~') {
        Some("") => Some(""),
        Some(rest) => rest.strip_prefix('/'),
        None => None,
    };
    let Some(rest) = rest else {
        return Ok(repo_root.join(raw));
    };
    let Some(home) = home else {
        bail!("{raw} is under a home directory, but this workspace's owner has none");
    };
    Ok(Path::new(home).join(rest))
}

/// A missing secret file is an error here, not a confusing 401 inside the build.
fn resolve_build_secrets(
    repo_root: &Path,
    home: Option<&str>,
    secrets: &[crate::resolver::NodeBuildSecret],
) -> Result<Vec<(String, std::path::PathBuf)>> {
    secrets
        .iter()
        .map(|secret| {
            let path = host_path(&secret.file, repo_root, home)?;
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
    use crate::orchestrator::testing::{test_graph, test_node};
    use crate::persistence::WorkspaceDb;
    use crate::resolver::NodeBuildSecret;
    use crate::resolver::port::PortConfig;

    #[test]
    fn a_context_size_reads_in_the_unit_a_human_would_pick() {
        assert_eq!(human_bytes(0), "0 B context");
        assert_eq!(human_bytes(900), "900 B context");
        assert_eq!(human_bytes(1024), "1.0 KiB context");
        assert_eq!(human_bytes(1536), "1.5 KiB context");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB context");
        assert_eq!(human_bytes(412 * 1024 * 1024), "412.0 MiB context");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB context");
    }

    #[test]
    fn a_build_duration_drops_its_tenths_once_they_stop_mattering() {
        use std::time::Duration;
        assert_eq!(human_duration(Duration::from_millis(1340)), "1.3s");
        assert_eq!(human_duration(Duration::from_millis(9949)), "9.9s");
        assert_eq!(human_duration(Duration::from_millis(10_400)), "10s");
        assert_eq!(human_duration(Duration::from_secs(59)), "59s");
        assert_eq!(human_duration(Duration::from_secs(60)), "1m 00s");
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

        let resolved =
            resolve_build_secrets(repo.path(), None, &[secret("npmrc", "ci/npmrc")]).unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "npmrc");
        assert_eq!(resolved[0].1, repo.path().join("ci/npmrc"));
    }

    #[test]
    fn a_missing_secret_file_fails_the_build_up_front() {
        let repo = tempfile::tempdir().unwrap();

        let err = resolve_build_secrets(repo.path(), None, &[secret("npmrc", "ci/npmrc")])
            .expect_err("a secret pointing at nothing must not build");

        let message = format!("{err:#}");
        assert!(message.contains("npmrc"), "got: {message}");
        assert!(message.contains("ci/npmrc"), "got: {message}");
    }

    #[test]
    fn a_secret_pointing_at_a_directory_is_rejected_too() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("ci")).unwrap();

        assert!(resolve_build_secrets(repo.path(), None, &[secret("npmrc", "ci")]).is_err());
    }

    #[test]
    fn a_tilde_path_is_under_the_owners_home_and_a_relative_one_under_the_repo() {
        let repo = Path::new("/ws/backend");
        let home = Some("/Users/me");
        let at = |raw| host_path(raw, repo, home).unwrap();

        assert_eq!(at("~/.aws"), PathBuf::from("/Users/me/.aws"));
        assert_eq!(at("~"), PathBuf::from("/Users/me"));
        assert_eq!(at("./db/seeds"), PathBuf::from("/ws/backend/./db/seeds"));
        assert_eq!(at("db/~/x"), PathBuf::from("/ws/backend/db/~/x"));
        assert_eq!(at("/etc/hosts"), PathBuf::from("/etc/hosts"));
        assert!(host_path("~/.aws", repo, None).is_err());
    }

    #[test]
    fn a_secret_file_under_home_is_read_from_the_owners_home() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".npmrc"), "token").unwrap();

        let resolved = resolve_build_secrets(
            repo.path(),
            home.path().to_str(),
            &[secret("npmrc", "~/.npmrc")],
        )
        .unwrap();

        assert_eq!(resolved[0].1, home.path().join(".npmrc"));
    }

    /// Joined to the repo, `~/.aws` made Docker create an empty `~` folder
    /// inside the checkout and mount that.
    #[tokio::test]
    async fn a_bind_mount_from_home_mounts_the_owners_home_not_the_repo() {
        let mut node = debuggable(None);
        node.local_path = Some("backend".to_string());
        node.volumes.push(crate::resolver::VolumeMount::Bind {
            host: "~/.aws".to_string(),
            container: "/root/.aws".to_string(),
            read_only: true,
        });
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        // What `fghj wire` records: the CLI's own `$HOME`, not root's.
        db.clone()
            .set_owner(crate::persistence::WorkspaceOwner {
                uid: 1000,
                gid: 1000,
                home: "/home/dev".to_string(),
                ssh_auth_sock: None,
            })
            .await
            .unwrap();
        let registry = Orchestrator::new(
            tmp.path().to_path_buf(),
            db,
            Arc::new(crate::docker::undialled_client()),
        );
        let graph = test_graph(vec![node.clone()], Vec::new());

        let spec = registry
            .resolve_node_spec(&graph, &node, false)
            .await
            .unwrap();

        assert_eq!(spec.binds[1], "/home/dev/.aws:/root/.aws:ro");
    }

    fn registry() -> (Orchestrator, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        let registry = Orchestrator::new(
            tmp.path().to_path_buf(),
            db,
            Arc::new(crate::docker::undialled_client()),
        );
        (registry, tmp)
    }

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
            .resolve_node_spec(&graph, node, false)
            .await
            .expect("resolving an image-backed node is pure")
    }

    #[tokio::test]
    async fn declaring_debug_publishes_the_port_and_injects_no_environment() {
        let spec = spec_for(&debuggable(Some(9229))).await;

        assert_eq!(spec.port_list, vec![("9229".to_string(), None)]);
        assert!(spec.env.is_empty(), "got: {:?}", spec.env);
    }

    /// `FGHJ_DEBUG_WAIT` is added after `spec_hash`, so it never shows as drift.
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
        assert!(spec.aliases.iter().any(|a| a.starts_with("inspector.")));
    }

    /// The trust mount must stay `:ro`: a writable one would let a container plant
    /// its own CA for every other container.
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

    #[tokio::test]
    async fn adding_debug_to_the_config_reads_as_drift() {
        let plain = debuggable(None);
        let debugging = debuggable(Some(9229));

        let before = crate::orchestrator::spec::spec_hash(&plain, &spec_for(&plain).await);
        let after = crate::orchestrator::spec::spec_hash(&debugging, &spec_for(&debugging).await);

        assert_ne!(before, after);
    }
}
