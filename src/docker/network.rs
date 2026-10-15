use std::collections::HashMap;
use std::net::Ipv4Addr;

use anyhow::{Context, Result};
use bollard::Docker;
use bollard::models::{Ipam, IpamConfig, NetworkCreateRequest};

/// Compose's network labels, so Docker Desktop and OrbStack can delete the
/// whole group including the network.
///
/// A new network is pinned to the subnet Docker picked for it, with dynamic
/// addresses confined to the upper half, so [`sidecar_address`] stays free
/// for the sidecar however often it's replaced: every node's `--dns` points
/// at it. Docker only honours a requested address on a network whose subnet
/// was given explicitly, hence create, read back, recreate.
pub async fn ensure_network(docker: &Docker, name: &str, project: &str) -> Result<()> {
    let mut labels = HashMap::new();
    labels.insert(
        "com.docker.compose.network".to_string(),
        "default".to_string(),
    );
    labels.insert(
        "com.docker.compose.project".to_string(),
        project.to_string(),
    );
    let request = NetworkCreateRequest {
        name: name.to_string(),
        labels: Some(labels),
        ..Default::default()
    };
    match create(docker, request.clone()).await? {
        Created::AlreadyThere => return Ok(()),
        Created::New => {}
    }

    let Some(picked) = ipv4_config(docker, name).await? else {
        return Ok(());
    };
    let Some(layout) = picked.subnet.as_deref().and_then(Layout::of) else {
        return Ok(());
    };
    let _ = docker.remove_network(name).await;
    let pinned = NetworkCreateRequest {
        ipam: Some(Ipam {
            config: Some(vec![IpamConfig {
                subnet: picked.subnet.clone(),
                gateway: picked.gateway.clone(),
                ip_range: Some(layout.dynamic_range),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..request.clone()
    };
    if let Err(e) = create(docker, pinned).await {
        // Everything still works unpinned; only a replaced sidecar can come
        // back on another address.
        eprintln!("fghjd: couldn't pin {name}'s subnet, leaving it to Docker: {e:#}");
        create(docker, request).await?;
    }
    Ok(())
}

/// The address reserved for the sidecar on `network`, or `None` for a network
/// fghj didn't pin (one created before pinning existed), where the sidecar
/// takes whatever Docker hands out.
pub async fn sidecar_address(docker: &Docker, network: &str) -> Result<Option<String>> {
    let Some(config) = ipv4_config(docker, network).await? else {
        return Ok(None);
    };
    let Some(layout) = config.subnet.as_deref().and_then(Layout::of) else {
        return Ok(None);
    };
    Ok(
        (config.ip_range.as_deref() == Some(layout.dynamic_range.as_str()))
            .then_some(layout.sidecar),
    )
}

enum Created {
    New,
    AlreadyThere,
}

async fn create(docker: &Docker, request: NetworkCreateRequest) -> Result<Created> {
    match docker.create_network(request).await {
        Ok(_) => Ok(Created::New),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 409, ..
        }) => Ok(Created::AlreadyThere),
        Err(e) => Err(e).context("docker create_network failed"),
    }
}

async fn ipv4_config(docker: &Docker, network: &str) -> Result<Option<IpamConfig>> {
    let inspected = docker
        .inspect_network(network, None)
        .await
        .context("docker inspect_network failed")?;
    Ok(inspected
        .ipam
        .and_then(|ipam| ipam.config)
        .unwrap_or_default()
        .into_iter()
        .find(|c| c.subnet.as_deref().and_then(Layout::of).is_some()))
}

/// How a pinned subnet is split: the sidecar at `.2` in the lower half,
/// everything Docker allocates in the upper half.
#[derive(Debug, PartialEq)]
struct Layout {
    sidecar: String,
    dynamic_range: String,
}

impl Layout {
    /// `None` for anything but an IPv4 subnet big enough to split, /29 or wider.
    fn of(subnet: &str) -> Option<Layout> {
        let (base, prefix) = subnet.split_once('/')?;
        let base: Ipv4Addr = base.parse().ok()?;
        let prefix: u32 = prefix.parse().ok()?;
        if !(1..=29).contains(&prefix) {
            return None;
        }
        let size = 1u32 << (32 - prefix);
        let base = u32::from(base) & !(size - 1);
        Some(Layout {
            sidecar: Ipv4Addr::from(base + 2).to_string(),
            dynamic_range: format!("{}/{}", Ipv4Addr::from(base + size / 2), prefix + 1),
        })
    }
}

pub async fn remove_network(docker: &Docker, name: &str) {
    let _ = docker.remove_network(name).await;
}

/// Container name by IPv4 address, for every container attached to `network`.
pub async fn network_container_ips(
    docker: &Docker,
    network: &str,
) -> Result<HashMap<String, String>> {
    let inspected = docker
        .inspect_network(network, None)
        .await
        .context("docker inspect_network failed")?;
    Ok(inspected
        .containers
        .unwrap_or_default()
        .into_values()
        .filter_map(|c| {
            let ip = c.ipv4_address?.split('/').next()?.to_string();
            Some((ip, c.name?))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sidecar_sits_below_the_half_docker_allocates_from() {
        assert_eq!(
            Layout::of("192.168.97.0/24"),
            Some(Layout {
                sidecar: "192.168.97.2".into(),
                dynamic_range: "192.168.97.128/25".into(),
            })
        );
        assert_eq!(
            Layout::of("172.18.0.0/16"),
            Some(Layout {
                sidecar: "172.18.0.2".into(),
                dynamic_range: "172.18.128.0/17".into(),
            })
        );
    }

    #[test]
    fn a_subnet_too_small_to_split_or_not_ipv4_is_left_alone() {
        assert_eq!(Layout::of("10.0.0.0/30"), None);
        assert_eq!(Layout::of("fd00::/64"), None);
        assert_eq!(Layout::of("garbage"), None);
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_new_network_keeps_the_sidecar_address_out_of_dynamic_allocation() {
        let docker = crate::daemon::connect_docker().expect("docker client");
        let name = format!("fghj-net-test-{}", crate::docker::rand_suffix());
        ensure_network(&docker, &name, &name).await.unwrap();

        let reserved = sidecar_address(&docker, &name).await.unwrap();
        let config = ipv4_config(&docker, &name).await.unwrap().unwrap();
        let layout = Layout::of(config.subnet.as_deref().unwrap()).unwrap();
        // Run again on an existing network: left as it is.
        ensure_network(&docker, &name, &name).await.unwrap();
        let again = sidecar_address(&docker, &name).await.unwrap();
        remove_network(&docker, &name).await;

        assert_eq!(reserved, Some(layout.sidecar));
        assert_eq!(again, reserved);
        assert_eq!(config.ip_range, Some(layout.dynamic_range));
    }
}
