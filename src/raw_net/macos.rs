//! macOS `pf`/`ifconfig` implementation of [`super::RawNetBackend`].
//!
//! Owns a dedicated top-level anchor `fghjd`, hooked once into `/etc/pf.conf`.
//! Each tick only reloads that anchor. Not nested under `com.apple/*`: `rdr`
//! rules two levels down never fired. Not a rewrite of the top-level ruleset
//! either: Docker Desktop injects live-only rules that a reload from disk
//! erases, breaking every published port.

use std::collections::BTreeSet;
use std::io::Write;
use std::net::Ipv4Addr;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use super::{RawNetBackend, RouteSpec};

const PF_CONF_PATH: &str = "/etc/pf.conf";
const ANCHOR_NAME: &str = "fghjd";
const HOOK_MARKER_BEGIN: &str =
    "# --- fghjd raw-net anchor hook (managed; do not edit — see raw_net::macos) ---";
const HOOK_MARKER_END: &str = "# --- end fghjd raw-net anchor hook ---";
const ANCHOR_HOOK_LINE: &str = "rdr-anchor \"fghjd\"";

pub struct MacosPfBackend {
    applied: Mutex<Vec<RouteSpec>>,
    /// Only `pfctl -d` on `clear()` if we enabled it.
    enabled_pf: Mutex<bool>,
}

impl MacosPfBackend {
    pub fn new() -> Self {
        let backend = Self {
            applied: Mutex::new(Vec::new()),
            enabled_pf: Mutex::new(false),
        };
        backend.self_heal_stray_aliases();
        backend
    }

    /// Removes pool aliases a crashed predecessor left; pool membership is the only
    /// ownership signal.
    fn self_heal_stray_aliases(&self) {
        for ip in current_pool_aliases() {
            let _ = remove_lo0_alias(ip);
        }
    }

    fn ensure_pf_enabled(&self) -> Result<()> {
        if pf_status_enabled() {
            return Ok(());
        }
        let output = Command::new("pfctl")
            .arg("-e")
            .output()
            .context("failed to run pfctl -e")?;
        if output.status.success() {
            *self.enabled_pf.lock().unwrap() = true;
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("already enabled") {
            Ok(())
        } else {
            bail!("pfctl -e failed: {}", stderr.trim());
        }
    }
}

impl Default for MacosPfBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl RawNetBackend for MacosPfBackend {
    fn apply(&self, routes: &[RouteSpec]) -> Result<()> {
        let mut desired = routes.to_vec();
        desired.sort();
        let mut applied = self.applied.lock().unwrap();

        let current_ips: BTreeSet<Ipv4Addr> = current_pool_aliases().into_iter().collect();
        let desired_ips: BTreeSet<Ipv4Addr> = desired.iter().map(|r| r.virtual_ip).collect();
        let (to_add, to_remove) = diff_ips(&current_ips, &desired_ips);
        for ip in to_add {
            add_lo0_alias(ip)?;
        }
        for ip in to_remove {
            remove_lo0_alias(ip)?;
        }

        if !desired.is_empty() {
            self.ensure_pf_enabled()?;
        }
        ensure_anchor_hook_installed()?;
        load_named_anchor_ruleset(&desired)?;

        *applied = desired;
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        for ip in current_pool_aliases() {
            let _ = remove_lo0_alias(ip);
        }
        let _ = load_named_anchor_ruleset(&[]);
        let _ = remove_anchor_hook();
        let mut enabled_pf = self.enabled_pf.lock().unwrap();
        if *enabled_pf {
            let _ = disable_pf();
            *enabled_pf = false;
        }
        *self.applied.lock().unwrap() = Vec::new();
        Ok(())
    }

    fn status(&self) -> Vec<RouteSpec> {
        self.applied.lock().unwrap().clone()
    }
}

fn diff_ips(
    current: &BTreeSet<Ipv4Addr>,
    desired: &BTreeSet<Ipv4Addr>,
) -> (Vec<Ipv4Addr>, Vec<Ipv4Addr>) {
    (
        desired.difference(current).copied().collect(),
        current.difference(desired).copied().collect(),
    )
}

fn in_pool(ip: Ipv4Addr) -> bool {
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
        .filter(|ip| in_pool(*ip))
        .collect()
}

fn current_pool_aliases() -> Vec<Ipv4Addr> {
    let Ok(output) = Command::new("ifconfig").arg("lo0").output() else {
        return Vec::new();
    };
    parse_pool_aliases(&String::from_utf8_lossy(&output.stdout))
}

/// Must lead every non-empty ruleset. Without it pf reverse-translates replies
/// from the redirect target, so a direct `127.0.0.1:<host_port>` connection (as
/// `web::proxy` makes) hangs in `SYN_RCVD`. Covers all of `127/8`, including
/// `PROXY_IP`; can't shadow a real rule, since every `rdr` targets the pool.
const LOOPBACK_EXEMPTION: &str = "no rdr on lo0 inet proto tcp from 127.0.0.0/8 to 127.0.0.0/8\n";

fn render_ruleset(routes: &[RouteSpec]) -> String {
    if routes.is_empty() {
        return String::new();
    }
    let mut out = String::from(LOOPBACK_EXEMPTION);
    for route in routes {
        out.push_str(&format!(
            "rdr pass on lo0 inet proto tcp from any to {} port {} -> 127.0.0.1 port {}\n",
            route.virtual_ip, route.container_port, route.host_port
        ));
    }
    out
}

fn pf_status_enabled() -> bool {
    Command::new("pfctl")
        .args(["-s", "info"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("Enabled"))
        .unwrap_or(false)
}

fn disable_pf() -> Result<()> {
    run(Command::new("pfctl").arg("-d"))
}

pub(super) fn add_lo0_alias(ip: Ipv4Addr) -> Result<()> {
    run(Command::new("ifconfig").args(["lo0", "alias", &ip.to_string(), "up"]))
}

pub(super) fn remove_lo0_alias(ip: Ipv4Addr) -> Result<()> {
    run(Command::new("ifconfig").args(["lo0", "-alias", &ip.to_string()]))
}

pub(super) fn has_lo0_alias(ip: Ipv4Addr) -> Result<bool> {
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
    Ok(lo0_has_alias(&String::from_utf8_lossy(&output.stdout), ip))
}

fn lo0_has_alias(ifconfig_output: &str, ip: Ipv4Addr) -> bool {
    let needle = ip.to_string();
    ifconfig_output.lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next() == Some("inet") && words.next() == Some(needle.as_str())
    })
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

/// pf requires translation rules before filtering rules. Falls back to before
/// the first filter `anchor`, or the end, for a customized `pf.conf`.
fn insert_after_translation_hooks(conf: &str, insert_lines: &[&str]) -> String {
    let lines: Vec<&str> = conf.lines().collect();
    let insert_at = lines
        .iter()
        .rposition(|l| {
            let t = l.trim_start();
            t.starts_with("nat-anchor") || t.starts_with("rdr-anchor")
        })
        .map(|i| i + 1)
        .or_else(|| {
            lines
                .iter()
                .position(|l| l.trim_start().starts_with("anchor"))
        })
        .unwrap_or(lines.len());

    let mut out: Vec<&str> = Vec::with_capacity(lines.len() + insert_lines.len());
    out.extend_from_slice(&lines[..insert_at]);
    out.extend_from_slice(insert_lines);
    out.extend_from_slice(&lines[insert_at..]);
    out.join("\n") + "\n"
}

/// Idempotent, so callers can skip writing when nothing changed.
fn install_anchor_hook(conf: &str) -> String {
    let cleaned = strip_managed_block(conf);
    insert_after_translation_hooks(
        &cleaned,
        &[HOOK_MARKER_BEGIN, ANCHOR_HOOK_LINE, HOOK_MARKER_END],
    )
}

fn ensure_anchor_hook_installed() -> Result<()> {
    let conf = std::fs::read_to_string(PF_CONF_PATH)
        .with_context(|| format!("failed to read {PF_CONF_PATH}"))?;
    let updated = install_anchor_hook(&conf);
    if updated != conf {
        std::fs::write(PF_CONF_PATH, &updated)
            .with_context(|| format!("failed to write {PF_CONF_PATH}"))?;
        reload_top_level_ruleset_from_disk()?;
    }
    Ok(())
}

/// Clean `clear()` only; a leftover hook with an empty anchor is harmless.
fn remove_anchor_hook() -> Result<()> {
    let conf = std::fs::read_to_string(PF_CONF_PATH)
        .with_context(|| format!("failed to read {PF_CONF_PATH}"))?;
    let stripped = strip_managed_block(&conf);
    if stripped != conf {
        std::fs::write(PF_CONF_PATH, &stripped)
            .with_context(|| format!("failed to write {PF_CONF_PATH}"))?;
        reload_top_level_ruleset_from_disk()?;
    }
    Ok(())
}

/// Only after editing `/etc/pf.conf` ourselves, and only from disk, so it can't
/// clobber anything beyond what's already persisted.
fn reload_top_level_ruleset_from_disk() -> Result<()> {
    run(Command::new("pfctl").args(["-f", PF_CONF_PATH]))
}

fn load_named_anchor_ruleset(routes: &[RouteSpec]) -> Result<()> {
    let ruleset = render_ruleset(routes);
    let mut child = Command::new("pfctl")
        .args(["-a", ANCHOR_NAME, "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn pfctl")?;
    child
        .stdin
        .take()
        .context("pfctl stdin unavailable")?
        .write_all(ruleset.as_bytes())
        .context("failed to write pf ruleset to pfctl stdin")?;
    let output = child
        .wait_with_output()
        .context("failed to wait for pfctl")?;
    if !output.status.success() {
        bail!(
            "pfctl -a {ANCHOR_NAME} -f - failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn run(cmd: &mut Command) -> Result<()> {
    let output = cmd
        .output()
        .with_context(|| format!("failed to run {cmd:?}"))?;
    if !output.status.success() {
        bail!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
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
        assert!(lo0_has_alias(output, PROXY_IP));
        assert!(lo0_has_alias(output, Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!lo0_has_alias(output, Ipv4Addr::new(127, 222, 0, 2)));
    }

    #[test]
    fn a_longer_address_sharing_our_prefix_is_not_a_match() {
        let output = "\tinet 127.222.0.10 netmask 0xff000000\n";
        assert!(!lo0_has_alias(output, PROXY_IP));
    }

    #[test]
    fn diff_ips_computes_additions_and_removals() {
        let current: BTreeSet<Ipv4Addr> =
            [Ipv4Addr::new(10, 222, 0, 1), Ipv4Addr::new(10, 222, 0, 2)].into();
        let desired: BTreeSet<Ipv4Addr> =
            [Ipv4Addr::new(10, 222, 0, 2), Ipv4Addr::new(10, 222, 0, 3)].into();

        let (add, remove) = diff_ips(&current, &desired);
        assert_eq!(add, vec![Ipv4Addr::new(10, 222, 0, 3)]);
        assert_eq!(remove, vec![Ipv4Addr::new(10, 222, 0, 1)]);
    }

    #[test]
    fn diff_ips_is_empty_when_sets_match() {
        let set: BTreeSet<Ipv4Addr> = [Ipv4Addr::new(10, 222, 0, 1)].into();
        let (add, remove) = diff_ips(&set, &set);
        assert!(add.is_empty());
        assert!(remove.is_empty());
    }

    #[test]
    fn render_ruleset_formats_one_rdr_line_per_route() {
        let routes = vec![RouteSpec {
            virtual_ip: Ipv4Addr::new(10, 222, 1, 1),
            container_port: 9000,
            host_port: 54321,
        }];
        assert_eq!(
            render_ruleset(&routes),
            "no rdr on lo0 inet proto tcp from 127.0.0.0/8 to 127.0.0.0/8\n\
             rdr pass on lo0 inet proto tcp from any to 10.222.1.1 port 9000 -> 127.0.0.1 port 54321\n"
        );
    }

    /// pf takes the first matching translation rule, so position matters.
    #[test]
    fn every_ruleset_exempts_loopback_to_loopback_before_any_redirect() {
        let routes = vec![
            RouteSpec {
                virtual_ip: Ipv4Addr::new(10, 222, 1, 1),
                container_port: 9000,
                host_port: 54321,
            },
            RouteSpec {
                virtual_ip: Ipv4Addr::new(10, 222, 1, 2),
                container_port: 5432,
                host_port: 54322,
            },
        ];
        let rendered = render_ruleset(&routes);
        let first = rendered.lines().next().expect("ruleset must not be empty");
        assert_eq!(first, LOOPBACK_EXEMPTION.trim_end());
        assert_eq!(
            rendered.lines().filter(|l| l.starts_with("no rdr")).count(),
            1,
            "one exemption for the whole anchor, not one per route"
        );
    }

    #[test]
    fn render_ruleset_of_no_routes_is_empty() {
        assert_eq!(render_ruleset(&[]), "");
    }

    #[test]
    fn parse_pool_aliases_extracts_only_pool_range_addresses() {
        let output = "\tinet 127.0.0.1 netmask 0xff000000\n\
                       \tinet 10.222.5.9 netmask 0xffffffff\n\
                       \tinet 192.168.1.1 netmask 0xffffff00\n";
        assert_eq!(
            parse_pool_aliases(output),
            vec![Ipv4Addr::new(10, 222, 5, 9)]
        );
    }

    #[test]
    fn in_pool_matches_only_10_222_range() {
        assert!(in_pool(Ipv4Addr::new(10, 222, 0, 1)));
        assert!(!in_pool(Ipv4Addr::new(10, 223, 0, 1)));
        assert!(!in_pool(Ipv4Addr::new(127, 0, 0, 1)));
    }

    const SAMPLE_PF_CONF: &str = "#\n\
        # Default PF configuration file.\n\
        #\n\
        \n\
        scrub-anchor \"com.apple/*\"\n\
        nat-anchor \"com.apple/*\"\n\
        rdr-anchor \"com.apple/*\"\n\
        dummynet-anchor \"com.apple/*\"\n\
        anchor \"com.apple/*\"\n\
        load anchor \"com.apple\" from \"/etc/pf.anchors/com.apple\"\n";

    #[test]
    fn install_anchor_hook_inserts_after_the_last_translation_hook() {
        let out = install_anchor_hook(SAMPLE_PF_CONF);
        let lines: Vec<&str> = out.lines().collect();
        let rdr_anchor_idx = lines
            .iter()
            .position(|l| *l == "rdr-anchor \"com.apple/*\"")
            .unwrap();
        let dummynet_idx = lines
            .iter()
            .position(|l| *l == "dummynet-anchor \"com.apple/*\"")
            .unwrap();
        assert_eq!(lines[rdr_anchor_idx + 1], HOOK_MARKER_BEGIN);
        assert_eq!(lines[rdr_anchor_idx + 2], ANCHOR_HOOK_LINE);
        assert_eq!(lines[rdr_anchor_idx + 3], HOOK_MARKER_END);
        assert_eq!(lines[rdr_anchor_idx + 4], "dummynet-anchor \"com.apple/*\"");
        assert!(dummynet_idx > rdr_anchor_idx);
    }

    #[test]
    fn strip_managed_block_restores_pristine_content() {
        let with_hook = install_anchor_hook(SAMPLE_PF_CONF);
        let restored = strip_managed_block(&with_hook);
        assert_eq!(restored, SAMPLE_PF_CONF);
    }

    #[test]
    fn install_anchor_hook_reapplied_does_not_duplicate_the_block() {
        let once = install_anchor_hook(SAMPLE_PF_CONF);
        let twice = install_anchor_hook(&once);
        assert_eq!(once, twice);
        assert_eq!(twice.matches(HOOK_MARKER_BEGIN).count(), 1);
    }

    #[test]
    fn install_anchor_hook_preserves_unrelated_content_around_the_block() {
        let out = install_anchor_hook(SAMPLE_PF_CONF);
        assert!(out.contains("# Default PF configuration file."));
        assert!(out.contains("load anchor \"com.apple\" from \"/etc/pf.anchors/com.apple\""));
    }

    #[test]
    fn install_anchor_hook_falls_back_to_before_the_first_anchor_line() {
        let conf = "anchor \"com.apple/*\"\nload anchor \"com.apple\" from \"/etc/pf.anchors/com.apple\"\n";
        let out = install_anchor_hook(conf);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], HOOK_MARKER_BEGIN);
        assert_eq!(lines[1], ANCHOR_HOOK_LINE);
        assert_eq!(lines[2], HOOK_MARKER_END);
        assert_eq!(lines[3], "anchor \"com.apple/*\"");
    }
}
