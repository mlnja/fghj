//! The environment's sidecar container that fronts its HTTP routes.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use super::route_table::{
    refresh_sidecar_ca_copy, sidecar_capture_token, sidecar_routes_dir, sidecar_routes_path,
};
use crate::docker;
use crate::state::EnvState;
use crate::web::capture::CAPTURE_PORT;

use super::naming::sidecar_name;
use super::registry::Orchestrator;

/// The Compose service label every sidecar carries, whatever its workspace:
/// how `fghj uninstall` finds them.
pub(crate) const SIDECAR_SERVICE: &str = "fghj-sidecar";

impl Orchestrator {
    /// Starts the environment's in-network TLS proxy sidecar unless it's already
    /// running. Returns its name and its address on `network`.
    pub(super) async fn ensure_sidecar(
        &self,
        workspace_name: &str,
        network: &str,
    ) -> Result<(String, String)> {
        let name = sidecar_name(workspace_name);
        let ip = self.ensure_sidecar_named(&name, network).await?;
        Ok((name, ip))
    }

    /// Brings back the environment's sidecar once an observation saw it gone or
    /// stopped, since every node's `--dns` points at it. Returns its address,
    /// which on a pinned network is the one it had before.
    pub async fn restore_sidecar(&self, env: &EnvState) -> Result<String> {
        self.ensure_sidecar_named(&env.sidecar_container_name, &env.network)
            .await
    }

    async fn sidecar_running(&self, name: &str) -> bool {
        matches!(
            docker::inspect_status(&self.docker, name).await,
            Ok(Some(s)) if s.status == "running"
        )
    }

    async fn ensure_sidecar_named(&self, name: &str, network: &str) -> Result<String> {
        let _guard = self.sidecar_lock.lock().await;
        let alive = self.sidecar_running(name).await;
        if alive && let Some(ip) = docker::inspect_network_ip(&self.docker, name, network).await? {
            return Ok(ip);
        }
        crate::sidecar_image::ensure_built(&self.docker).await?;
        let address = docker::sidecar_address(&self.docker, network).await?;

        let routes_dir = sidecar_routes_dir(network);
        std::fs::create_dir_all(&routes_dir)
            .with_context(|| format!("failed to create {routes_dir:?}"))?;
        let routes_path = sidecar_routes_path(network);
        if !routes_path.exists() {
            std::fs::write(&routes_path, b"[]")
                .with_context(|| format!("failed to create {routes_path:?}"))?;
        }
        sidecar_capture_token(network)?;

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

        // Docker binds it to the host's 127.0.0.1 on a port of its choosing;
        // fghjd looks it up when it starts pulling.
        let ports = [(CAPTURE_PORT.to_string(), None)];
        docker::run_container(
            &self.docker,
            &docker::RunOpts {
                name,
                network,
                aliases: &[],
                ipv4_address: address.as_deref(),
                env: &[],
                ports: &ports,
                image: &crate::sidecar_image::image_tag(),
                command: &[],
                project: network,
                service_name: SIDECAR_SERVICE,
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

        docker::inspect_network_ip(&self.docker, name, network)
            .await?
            .context("sidecar proxy started but has no address on its own network")
    }
}
