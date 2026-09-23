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
            _ => {
                let build = node.build.clone().unwrap_or(crate::resolver::NodeBuild {
                    context: ".".to_string(),
                    dockerfile: "Dockerfile".to_string(),
                    args: BTreeMap::new(),
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
                    let build_dir = repo_root.join(&build.context);
                    self.record_event(
                        run_id,
                        &node.id,
                        "start",
                        "building image",
                        "running",
                        Some(tag.clone()),
                    )
                    .await;
                    if let Err(e) = docker::build_image(
                        &self.docker,
                        &build_dir,
                        &build.dockerfile,
                        &tag,
                        node.platform.as_deref(),
                    )
                    .await
                    {
                        self.record_event(
                            run_id,
                            &node.id,
                            "start",
                            "building image",
                            "error",
                            Some(format!("{e:#}")),
                        )
                        .await;
                        return Err(e);
                    }
                    self.record_event(run_id, &node.id, "start", "building image", "ok", None)
                        .await;
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

        let port_list: Vec<(String, Option<u16>)> = node
            .ports
            .iter()
            .map(|(port, cfg)| (port.clone(), cfg.host_port))
            .collect();

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
                } => {
                    let volume_name =
                        derive_volume_name(name, scope, &graph.workspace_name, run_id);
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
