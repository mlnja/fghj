//! The per-run sidecar container that fronts a run's HTTP routes.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use super::route_table::{refresh_sidecar_ca_copy, sidecar_routes_dir, sidecar_routes_path};
use crate::docker;
use crate::util::label::sanitize_label;

use super::registry::RunRegistry;

impl RunRegistry {
    /// Starts (or confirms already running) this run's in-network TLS proxy
    /// sidecar — one per run, on that run's own docker network, never
    /// shared across workspaces/runs, so a container inside the network can
    /// reach a sibling's `*.fghj.internal` name with the same addressing a
    /// browser outside the network gets from the host-side proxy. Returns
    /// its deterministic container name and its address on `network`.
    ///
    /// Idempotent, like `ensure_running`'s per-node liveness check: checked
    /// directly against Docker rather than trusted from `RunState`, since
    /// the sidecar can be stopped/removed out-of-band just like any other
    /// container.
    pub(super) async fn ensure_sidecar(
        &self,
        workspace_name: &str,
        run_id: &str,
        network: &str,
    ) -> Result<(String, String)> {
        let name = format!("fghj-{}-{}-sidecar", sanitize_label(workspace_name), run_id);

        let alive = matches!(
            docker::inspect_status(&self.docker, &name, "").await,
            Ok(Some(s)) if s.status == "running"
        );
        if alive && let Some(ip) = docker::inspect_network_ip(&self.docker, &name, network).await? {
            return Ok((name, ip));
        }
        // A stopped-but-not-removed sidecar from a previous run would
        // otherwise collide with create_container's fixed name.
        docker::stop_and_remove(&self.docker, &name).await;

        crate::sidecar_image::ensure_built(&self.docker).await?;

        let routes_dir = sidecar_routes_dir(network);
        std::fs::create_dir_all(&routes_dir)
            .with_context(|| format!("failed to create {routes_dir:?}"))?;
        let routes_path = sidecar_routes_path(network);
        if !routes_path.exists() {
            std::fs::write(&routes_path, b"[]")
                .with_context(|| format!("failed to create {routes_path:?}"))?;
        }

        // Canonicalized, not the literal `/var/lib/...` path: macOS's `/var`
        // is a symlink to `/private/var`, and OrbStack's bind-mount source
        // resolution doesn't follow it — a source of `/var/lib/fghjd/...`
        // silently resolves inside the Docker VM's own filesystem instead of
        // the real host path, so the mounted directory shows up empty
        // instead of erroring. The already-resolved `/private/var/lib/...`
        // form mounts correctly.
        let routes_dir = std::fs::canonicalize(&routes_dir)
            .with_context(|| format!("failed to canonicalize {routes_dir:?}"))?;
        let ca_dir = refresh_sidecar_ca_copy()?;
        let ca_dir = std::fs::canonicalize(ca_dir)
            .context("failed to canonicalize the sidecar CA directory")?;

        // Sibling mounts, not nested — binding `ca` underneath an already
        // bind-mounted, read-only `/etc/fghj-sidecar` fails outright (the
        // container runtime can't create a mountpoint inside a read-only
        // mount). `/etc/fghj-sidecar` itself is never bind-mounted from the
        // host, so the runtime creates it as an ordinary (writable)
        // directory in the container's own layer, and both binds attach
        // under it independently.
        let binds = vec![
            format!("{}:/etc/fghj-sidecar/routes:ro", routes_dir.display()),
            format!("{}:/etc/fghj-sidecar/ca:ro", ca_dir.display()),
        ];

        docker::run_container(
            &self.docker,
            &docker::RunOpts {
                name: &name,
                network,
                aliases: &[],
                env: &[],
                ports: &[],
                image: &crate::sidecar_image::image_tag(),
                command: &[],
                project: network,
                service_name: "fghj-sidecar",
                binds: &binds,
                restart_policy: "unless-stopped",
                user: None,
                working_dir: None,
                labels: &BTreeMap::new(),
                cap_add: &[],
                cap_drop: &[],
                privileged: false,
                extra_hosts: &[],
                dns: &[],
                healthcheck: None,
                platform: None,
            },
        )
        .await
        .context("failed to start this run's sidecar proxy")?;

        let ip = docker::inspect_network_ip(&self.docker, &name, network)
            .await?
            .context("sidecar proxy started but has no address on its own network")?;
        Ok((name, ip))
    }
}
