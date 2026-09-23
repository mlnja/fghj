use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::persistence::{self};
use crate::state::{ContainerInfo, RunState};

/// One entry in the route table `write_route_table` persists for a run's
/// sidecar proxy to poll (see `src/bin/fghj-sidecar.rs`'s own
/// field-name-matching `RouteFileEntry`, kept as a separate type so that
/// binary doesn't need to depend on this module at all). Needs no raw
/// IP/container-name derivation: `connect_host` is the container's
/// `fghj.raw.internal` domain, a real Docker network alias (see
/// `resolve_node_spec`'s `aliases`) that Docker's own embedded per-network
/// DNS resolves for any container on the network, including the sidecar
/// itself — the `fghj.internal` domain (`lookup`) is deliberately *not* a
/// Docker alias on the node's own container anymore, since the sidecar
/// itself is what needs to own that name's resolution.
#[derive(Debug, PartialEq, Serialize)]
pub(crate) struct RouteFileEntry {
    lookup: String,
    wildcard: bool,
    connect_host: String,
    connect_port: u16,
}

/// The pure part of `write_route_table` — every routable domain across
/// `containers`, connecting via each container's own `raw_domain` (see
/// `RouteFileEntry`'s doc comment for why). Split out from the actual file
/// write so it can be unit-tested without touching `/var/lib/fghjd`, which
/// is root-owned in production.
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

pub(crate) fn sidecar_ca_dir() -> PathBuf {
    persistence::fghjd_root().join("sidecar-ca")
}

/// A world-readable copy of the CA cert+key, refreshed on every sidecar
/// (re)creation, kept separate from the real `daemon::ca_dir()` — `fghjd`
/// runs as root and the real `ca-key.pem` is deliberately `0600`
/// root-owned, but Docker Desktop/OrbStack's bind-mount sharing on macOS is
/// brokered by a process running as the logged-in user, not root: even a
/// container claiming to run as `root` can't read a `0600` root-owned file
/// through that bridge, since the permission check happens on the host side
/// against the real user, before the request ever reaches the container's
/// own UID namespace. Mounting the CA into a container at all is already
/// the accepted tradeoff for this feature (see `fghj-sidecar.rs`'s own
/// doc comment); this only has to be readable by whoever is already running
/// `sudo fghjd` on this machine, which is a strictly smaller exposure.
pub(crate) fn refresh_sidecar_ca_copy() -> Result<PathBuf> {
    let dir = sidecar_ca_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {dir:?}"))?;

    let src_dir = crate::daemon::ca_dir();
    for (src, dest_name) in [
        (crate::ca::ca_cert_path(&src_dir), "ca-cert.pem"),
        (crate::ca::ca_key_path(&src_dir), "ca-key.pem"),
    ] {
        let bytes = std::fs::read(&src).with_context(|| format!("failed to read {src:?}"))?;
        let dest = dir.join(dest_name);
        std::fs::write(&dest, bytes).with_context(|| format!("failed to write {dest:?}"))?;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o644))
            .with_context(|| format!("failed to set permissions on {dest:?}"))?;
    }

    Ok(dir)
}

/// Regenerates the route table this run's sidecar proxy polls from, from
/// `state.containers` alone — every routable domain across every container
/// this run knows about, whether or not that container's own status is
/// currently `running` (matching a stopped-then-restarted route staying
/// valid the moment the container comes back). Best-effort: a write failure
/// here shouldn't fail the start/stop call it's riding along on, since the
/// next lifecycle call for this run retries it anyway.
pub(crate) fn write_route_table(state: &RunState) -> Result<()> {
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
            }]
        );
    }
}
