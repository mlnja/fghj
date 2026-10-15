use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::persistence::{self};
use crate::state::{ContainerInfo, EnvState};

/// Must match `RouteFileEntry` in `src/bin/fghj-sidecar.rs`. `connect_host` is
/// the container's raw domain, a Docker alias the sidecar can resolve.
#[derive(Debug, PartialEq, Serialize)]
pub(crate) struct RouteFileEntry {
    lookup: String,
    wildcard: bool,
    connect_host: String,
    connect_port: u16,
    node_id: String,
}

pub(crate) fn route_file_entries<'a>(
    containers: impl IntoIterator<Item = &'a ContainerInfo>,
) -> Vec<RouteFileEntry> {
    containers
        .into_iter()
        .flat_map(|c| {
            c.desired.routes.iter().filter_map(move |r| {
                let connect_port = r.container_port.split('/').next()?.parse().ok()?;
                Some(RouteFileEntry {
                    lookup: r.domain.clone(),
                    wildcard: r.wildcard,
                    connect_host: c.desired.raw_domain.clone(),
                    connect_port,
                    node_id: c.node_id.clone(),
                })
            })
        })
        .collect()
}

pub(crate) fn sidecar_routes_dir(network: &str) -> PathBuf {
    persistence::fghjd_root().join("runs").join(network)
}

pub(crate) fn sidecar_routes_path(network: &str) -> PathBuf {
    sidecar_routes_dir(network).join("routes.json")
}

/// Created once per network and read on every capture request, so a running
/// sidecar keeps accepting the same one across fghjd restarts.
pub(crate) fn sidecar_capture_token(network: &str) -> Result<String> {
    let path = sidecar_routes_dir(network).join(crate::web::capture::TOKEN_FILE);
    if let Ok(token) = std::fs::read_to_string(&path)
        && !token.trim().is_empty()
    {
        return Ok(token.trim().to_string());
    }
    let token = crate::web::capture::new_token()?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &token).with_context(|| format!("failed to write {tmp:?}"))?;
    // World-readable like the rest of this folder, or it can't be mounted;
    // it only keeps out other containers and web pages, not local users.
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))
        .with_context(|| format!("failed to set permissions on {tmp:?}"))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("failed to rename into {path:?}"))?;
    Ok(token)
}

pub(crate) fn sidecar_ca_dir() -> PathBuf {
    persistence::fghjd_root().join("sidecar-ca")
}

/// A world-readable copy of the signing CA for the sidecar: Docker Desktop and
/// OrbStack check bind-mount permissions as the logged-in user, so a `0600`
/// root file can't be mounted. The signing CA is name-constrained to fghj's
/// zones; see `ca::ensure_signing_ca`. Also run at startup to overwrite a root
/// key older installs left here.
pub(crate) fn refresh_sidecar_ca_copy() -> Result<PathBuf> {
    let dir = sidecar_ca_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {dir:?}"))?;

    // The signing CA, never the root.
    let src_dir = crate::daemon::ca_dir();
    for (src, dest_name) in [
        (crate::web::ca::signing_cert_path(&src_dir), "ca-cert.pem"),
        (crate::web::ca::signing_key_path(&src_dir), "ca-key.pem"),
    ] {
        let bytes = std::fs::read(&src).with_context(|| format!("failed to read {src:?}"))?;
        let dest = dir.join(dest_name);
        std::fs::write(&dest, bytes).with_context(|| format!("failed to write {dest:?}"))?;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o644))
            .with_context(|| format!("failed to set permissions on {dest:?}"))?;
    }

    Ok(dir)
}

/// Regenerates the sidecar's route table from `state.containers`, stopped
/// containers included so a route is valid the moment one comes back.
pub(crate) fn write_route_table(state: &EnvState) -> Result<()> {
    let dir = sidecar_routes_dir(&state.network);
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {dir:?}"))?;

    let entries = route_file_entries(state.containers.values());

    let path = sidecar_routes_path(&state.network);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&entries)?)
        .with_context(|| format!("failed to write {tmp:?}"))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("failed to rename into {path:?}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ContainerDesired, ContainerObserved, PortRoute};

    #[test]
    fn route_file_entries_connect_via_the_raw_domain_not_the_http_one() {
        let containers = vec![ContainerInfo {
            node_id: "svc".to_string(),
            desired: ContainerDesired {
                running: true,
                container_name: "fghj-test-svc".to_string(),
                domain: "svc.demo.fghj.internal".to_string(),
                raw_domain: "svc.demo.fghj.raw.internal".to_string(),
                routes: vec![PortRoute {
                    domain: "svc.demo.fghj.internal".to_string(),
                    host_port: 8080,
                    wildcard: false,
                    https: true,
                    container_port: "8080".to_string(),
                }],
                additional_hosts: Vec::new(),
                status_port: Some("8080".to_string()),
                config_hash: String::new(),
                source: None,
                terminating: false,
                debug_wait: false,
            },
            observed: ContainerObserved::default(),
            pending_action: None,
        }];

        let entries = route_file_entries(&containers);
        assert_eq!(
            entries,
            vec![RouteFileEntry {
                lookup: "svc.demo.fghj.internal".to_string(),
                wildcard: false,
                connect_host: "svc.demo.fghj.raw.internal".to_string(),
                connect_port: 8080,
                node_id: "svc".to_string(),
            }]
        );
    }
}
