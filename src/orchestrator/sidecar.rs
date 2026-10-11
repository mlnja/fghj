//! The environment's sidecar container that fronts its HTTP routes.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use super::route_table::{refresh_sidecar_ca_copy, sidecar_routes_dir, sidecar_routes_path};
use crate::docker;

use super::naming::sidecar_name;
use super::registry::Orchestrator;

impl Orchestrator {
    /// Starts the environment's in-network TLS proxy sidecar unless it's already
    /// running. Returns its name and its address on `network`.
    pub(super) async fn ensure_sidecar(
        &self,
        workspace_name: &str,
        network: &str,
    ) -> Result<(String, String)> {
        let name = sidecar_name(workspace_name);

        let alive = matches!(
            docker::inspect_status(&self.docker, &name, "").await,
            Ok(Some(s)) if s.status == "running"
        );
        if alive && let Some(ip) = docker::inspect_network_ip(&self.docker, &name, network).await? {
            return Ok((name, ip));
        }
        crate::sidecar_image::ensure_built(&self.docker).await?;

        let routes_dir = sidecar_routes_dir(network);
        std::fs::create_dir_all(&routes_dir)
            .with_context(|| format!("failed to create {routes_dir:?}"))?;
        let routes_path = sidecar_routes_path(network);
        if !routes_path.exists() {
            std::fs::write(&routes_path, b"[]")
                .with_context(|| format!("failed to create {routes_path:?}"))?;
        }

        // Canonicalized: OrbStack doesn't follow macOS's `/var` symlink and would
        // mount an empty directory.
        let routes_dir = std::fs::canonicalize(&routes_dir)
            .with_context(|| format!("failed to canonicalize {routes_dir:?}"))?;
        let ca_dir = refresh_sidecar_ca_copy()?;
        let ca_dir = std::fs::canonicalize(ca_dir)
            .context("failed to canonicalize the sidecar CA directory")?;

        // Sibling mounts: a bind can't be nested inside a read-only bind.
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
                // Stateless, so a short grace period keeps teardown snappy.
                stop_signal: None,
                stop_grace_period: 2,
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
