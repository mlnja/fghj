//! `lo0` aliases: macOS assigns only `127.0.0.1` to loopback, so every other
//! address fghj listens on or NATs from has to be added by hand.

use std::net::Ipv4Addr;
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::run;
use crate::platform::Loopback;

pub(super) struct Lo0;

impl Loopback for Lo0 {
    fn add_alias(&self, ip: Ipv4Addr) -> Result<()> {
        add_alias(ip)
    }

    fn remove_alias(&self, ip: Ipv4Addr) -> Result<()> {
        remove_alias(ip).or(Ok(()))
    }

    fn alias_present(&self, ip: Ipv4Addr) -> Option<Result<bool>> {
        Some(addresses().map(|output| has_alias(&output, ip)))
    }

    fn interface(&self) -> &'static str {
        "lo0"
    }

    fn needs_aliases(&self) -> bool {
        true
    }
}

pub(super) fn add_alias(ip: Ipv4Addr) -> Result<()> {
    run(Command::new("ifconfig").args(["lo0", "alias", &ip.to_string(), "up"]))
}

pub(super) fn remove_alias(ip: Ipv4Addr) -> Result<()> {
    run(Command::new("ifconfig").args(["lo0", "-alias", &ip.to_string()]))
}

/// `ifconfig lo0`'s output.
pub(super) fn addresses() -> Result<String> {
    let output = Command::new("ifconfig")
        .arg("lo0")
        .output()
        .context("could not run `ifconfig lo0`")?;
    if !output.status.success() {
        bail!(
            "`ifconfig lo0` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn has_alias(ifconfig_output: &str, ip: Ipv4Addr) -> bool {
    let needle = ip.to_string();
    ifconfig_output.lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next() == Some("inet") && words.next() == Some(needle.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::proxy::PROXY_IP;

    #[test]
    fn lo0_alias_is_found_by_exact_address_not_substring() {
        let output = "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384\n\
                      \tinet 127.0.0.1 netmask 0xff000000\n\
                      \tinet 127.222.0.1 netmask 0xff000000\n";
        assert!(has_alias(output, PROXY_IP));
        assert!(has_alias(output, Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!has_alias(output, Ipv4Addr::new(127, 222, 0, 2)));
    }

    #[test]
    fn a_longer_address_sharing_our_prefix_is_not_a_match() {
        let output = "\tinet 127.222.0.10 netmask 0xff000000\n";
        assert!(!has_alias(output, PROXY_IP));
    }
}
