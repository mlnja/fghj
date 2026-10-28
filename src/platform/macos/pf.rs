//! What fghjd up to 0.1.53 left for raw-zone ports: `rdr` rules in a `pf`
//! anchor `fghjd`, a hook for it in `/etc/pf.conf`, and a `lo0` alias per
//! virtual IP in `10.222/16`. Removed on start, since `raw_net::forward`
//! replaced all of it: any program that reloads pf (Internet Sharing loads a
//! ruleset with `set skip on lo0`) could turn the rules off without a trace.

use std::net::Ipv4Addr;
use std::process::Command;

use anyhow::{Context, Result};

use super::lo0;
use crate::platform::LegacyNat;

const PF_CONF_PATH: &str = "/etc/pf.conf";
const ANCHOR_NAME: &str = "fghjd";
const HOOK_MARKER_BEGIN: &str =
    "# --- fghjd raw-net anchor hook (managed; do not edit — see raw_net::macos) ---";
const HOOK_MARKER_END: &str = "# --- end fghjd raw-net anchor hook ---";

pub(super) struct RetiredPf;

impl LegacyNat for RetiredPf {
    fn retire(&self) -> Result<()> {
        for ip in lo0::addresses()
            .map(|out| parse_pool_aliases(&out))
            .unwrap_or_default()
        {
            let _ = lo0::remove_alias(ip);
        }

        let conf = match std::fs::read_to_string(PF_CONF_PATH) {
            Ok(conf) => conf,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("failed to read {PF_CONF_PATH}")),
        };
        if !conf.contains(HOOK_MARKER_BEGIN) {
            return Ok(());
        }
        // Flushing the anchor is enough to stop the rules. The ruleset isn't
        // reloaded from disk: that would drop the anchors other programs add
        // live (Internet Sharing, Docker Desktop). The hook left in the live
        // ruleset points at an empty anchor and is gone at the next reload.
        let _ = Command::new("pfctl")
            .args(["-a", ANCHOR_NAME, "-F", "all"])
            .output();
        std::fs::write(PF_CONF_PATH, strip_managed_block(&conf))
            .with_context(|| format!("failed to write {PF_CONF_PATH}"))
    }
}

fn in_old_pool(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 10 && octets[1] == 222
}

fn parse_pool_aliases(ifconfig_output: &str) -> Vec<Ipv4Addr> {
    ifconfig_output
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("inet ")?;
            rest.split_whitespace().next()?.parse::<Ipv4Addr>().ok()
        })
        .filter(|ip| in_old_pool(*ip))
        .collect()
}

fn strip_managed_block(conf: &str) -> String {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in conf.lines() {
        if line == HOOK_MARKER_BEGIN {
            in_block = true;
            continue;
        }
        if line == HOOK_MARKER_END {
            in_block = false;
            continue;
        }
        if !in_block {
            out.push(line);
        }
    }
    out.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_old_pool_is_removed() {
        let output = "\tinet 127.0.0.1 netmask 0xff000000\n\
                       \tinet 10.222.5.9 netmask 0xffffffff\n\
                       \tinet 127.223.5.9 netmask 0xff000000\n\
                       \tinet 10.223.0.1 netmask 0xffffff00\n";
        assert_eq!(
            parse_pool_aliases(output),
            vec![Ipv4Addr::new(10, 222, 5, 9)]
        );
    }

    #[test]
    fn stripping_the_hook_restores_the_stock_pf_conf() {
        let stock = "scrub-anchor \"com.apple/*\"\n\
                     nat-anchor \"com.apple/*\"\n\
                     rdr-anchor \"com.apple/*\"\n\
                     dummynet-anchor \"com.apple/*\"\n\
                     anchor \"com.apple/*\"\n\
                     load anchor \"com.apple\" from \"/etc/pf.anchors/com.apple\"\n";
        let hooked = stock.replace(
            "rdr-anchor \"com.apple/*\"\n",
            &format!(
                "rdr-anchor \"com.apple/*\"\n{HOOK_MARKER_BEGIN}\nrdr-anchor \"fghjd\"\n{HOOK_MARKER_END}\n"
            ),
        );
        assert_eq!(strip_managed_block(&hooked), stock);
    }
}
