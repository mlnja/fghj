//! Linux split DNS through systemd-resolved: a dummy link `fghj0` whose DNS
//! server is fghjd and whose routing domains are fghj's zones.
//!
//! A link of its own because resolved's global `DNS=` is always a default
//! route, so it would see every lookup on the machine; per-link servers only
//! get the domains the link claims. (`dns-delegate.d` does this without a
//! link, but only from systemd 258.) The settings are runtime-only, kept by
//! resolved across its own restarts, and gone with the link.

use std::process::Command;

use anyhow::{Context, Result};

use super::super::run;
use crate::daemon_log;
use crate::platform::{DnsRouting, OsRouting};

const LINK: &str = "fghj0";

/// resolved gives a link no DNS scope until it holds an address wider than
/// link scope, so the link needs one. Inside the proxy's `/16`, which on
/// Linux is all local already, so it shadows nothing.
const LINK_ADDRESS: &str = "127.222.0.53/32";

pub(super) struct ResolvedLink;

impl DnsRouting for ResolvedLink {
    fn sync(&self, port: u16, zones: &[&str]) -> Result<()> {
        ensure_link()?;
        let server = format!("127.0.0.1:{port}");
        resolvectl(&["dns", LINK, &server])?;
        let domains: Vec<String> = zones.iter().map(|z| format!("~{z}")).collect();
        let mut args = vec!["domain", LINK];
        args.extend(domains.iter().map(String::as_str));
        resolvectl(&args)?;
        // `.internal` is provably absent from the signed root, so validating
        // would reject every answer fghjd gives.
        for setting in ["default-route", "llmnr", "mdns", "dnssec"] {
            resolvectl(&[setting, LINK, "no"])?;
        }
        daemon_log::info(format!(
            "fghjd: routed {} through systemd-resolved to {server}",
            zones.join(", ")
        ));
        Ok(())
    }

    fn clear(&self) {
        let _ = run(Command::new("ip").args(["link", "del", LINK]));
    }

    fn status(&self) -> Option<OsRouting> {
        let read = |what: &str| {
            Command::new("resolvectl")
                .args([what, LINK])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        };
        let zones = match (read("dns"), read("domain")) {
            (Some(dns), Some(domain)) => routed_zones(&dns, &domain),
            _ => Vec::new(),
        };
        Some(OsRouting {
            location: "systemd-resolved link fghj0",
            zones,
        })
    }

    fn footprint(&self) -> Option<&'static str> {
        Some("the fghj0 link, which routes fghj's zones in systemd-resolved")
    }
}

fn ensure_link() -> Result<()> {
    if run(Command::new("ip").args(["link", "show", LINK])).is_err() {
        run(Command::new("ip").args(["link", "add", LINK, "type", "dummy"]))?;
    }
    run(Command::new("ip").args(["link", "set", LINK, "up"]))?;
    run(Command::new("ip").args([
        "addr",
        "replace",
        LINK_ADDRESS,
        "dev",
        LINK,
        "scope",
        "global",
    ]))
}

fn resolvectl(args: &[&str]) -> Result<()> {
    run(Command::new("resolvectl").args(args))
        .context("fghj routes its zones through systemd-resolved; is it running?")
}

/// The text after `Link 3 (fghj0):`, which is how `resolvectl dns|domain
/// <link>` prints a link's settings.
fn link_values(output: &str) -> Vec<&str> {
    output
        .lines()
        .find_map(|line| line.split_once("):").map(|(_, rest)| rest))
        .map(|rest| rest.split_whitespace().collect())
        .unwrap_or_default()
}

/// Only zones pointed at fghjd's address count, so another tool's server on
/// the link doesn't pass for fghjd's.
fn routed_zones(dns: &str, domain: &str) -> Vec<(String, u16)> {
    let Some(port) = link_values(dns).iter().find_map(|server| {
        server
            .strip_prefix("127.0.0.1:")
            .and_then(|port| port.parse::<u16>().ok())
    }) else {
        return Vec::new();
    };
    let mut zones: Vec<(String, u16)> = link_values(domain)
        .iter()
        .filter_map(|d| d.strip_prefix('~'))
        .map(|zone| (zone.to_string(), port))
        .collect();
    zones.sort();
    zones
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routed_zones_reads_back_resolvectl() {
        let dns = "Link 3 (fghj0): 127.0.0.1:5399\n";
        let domain = "Link 3 (fghj0): ~fghj.raw.internal ~fghj.internal\n";
        assert_eq!(
            routed_zones(dns, domain),
            [
                ("fghj.internal".to_string(), 5399),
                ("fghj.raw.internal".to_string(), 5399)
            ]
        );
    }

    #[test]
    fn a_link_with_no_server_routes_nothing() {
        assert!(routed_zones("Link 3 (fghj0):\n", "Link 3 (fghj0): ~fghj.internal\n").is_empty());
    }

    #[test]
    fn a_server_that_isnt_fghjd_routes_nothing() {
        let dns = "Link 3 (fghj0): 192.168.1.1\n";
        assert!(routed_zones(dns, "Link 3 (fghj0): ~fghj.internal\n").is_empty());
    }

    /// Search domains, without `~`, would also send single-label names here.
    #[test]
    fn only_routing_domains_count() {
        let dns = "Link 3 (fghj0): 127.0.0.1:53\n";
        let domain = "Link 3 (fghj0): corp.example ~fghj.internal\n";
        assert_eq!(
            routed_zones(dns, domain),
            [("fghj.internal".to_string(), 53)]
        );
    }

    /// Against the real system resolver: root on a systemd-resolved host,
    /// `cargo test -- --ignored resolved_routes_a_zone_end_to_end`.
    #[test]
    #[ignore]
    fn resolved_routes_a_zone_end_to_end() {
        ResolvedLink
            .sync(5399, &["fghj.internal", "fghj.raw.internal"])
            .unwrap();
        let status = ResolvedLink.status().unwrap();
        assert_eq!(
            status.zones,
            [
                ("fghj.internal".to_string(), 5399),
                ("fghj.raw.internal".to_string(), 5399)
            ]
        );
        ResolvedLink.sync(5400, &["fghj.internal"]).unwrap();
        assert_eq!(
            ResolvedLink.status().unwrap().zones,
            [("fghj.internal".to_string(), 5400)]
        );
        ResolvedLink.clear();
        assert!(ResolvedLink.status().unwrap().zones.is_empty());
    }
}
