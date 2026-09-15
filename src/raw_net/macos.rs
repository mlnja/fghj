//! macOS `pf`/`ifconfig` implementation of [`super::RawNetBackend`].
//!
//! Piggybacks on stock macOS's default `/etc/pf.conf`, which already
//! wildcard-hooks `rdr-anchor "com.apple/*"` / `nat-anchor "com.apple/*"` /
//! `anchor "com.apple/*"` — the same trick Docker Desktop and various
//! VPN/proxy tools use to load dynamic pf rules without ever editing
//! `/etc/pf.conf` itself, and without risk of clobbering the user's own
//! rules.
//!
//! Every command-running function here is a thin wrapper around pure,
//! independently-testable logic (`diff_ips`, `render_ruleset`,
//! `parse_pool_aliases`, `in_pool`) — the actual `Command` calls are the only
//! part that needs macOS and root to exercise for real.

use std::collections::BTreeSet;
use std::io::Write;
use std::net::Ipv4Addr;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use super::{RawNetBackend, RouteSpec};

/// Anchor path — see the module doc for why this exact name.
const ANCHOR: &str = "com.apple/fghjd";

pub struct MacosPfBackend {
    /// Last-applied route set, so `apply` can skip `pfctl`/`ifconfig`
    /// entirely on a no-op tick — matches `hosts_file::sync`'s
    /// write-only-if-changed idempotency, instead of touching live OS
    /// firewall/interface state every reconcile tick for no reason.
    applied: Mutex<Vec<RouteSpec>>,
    /// Whether *this* process was the one that enabled pf (`pfctl -e`) —
    /// only ever `pfctl -d` on `clear()` if so, never force-disabling pf
    /// that the user or some other tool already had on.
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

    /// Removes any pool-range `lo0` alias left behind by a previous, e.g.
    /// crashed, `fghjd` process. This process has no in-memory record of
    /// what a prior instance applied, so pool membership — the same
    /// ownership predicate `apply`'s own diffing uses — is the only signal
    /// available.
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
        if *applied == desired {
            return Ok(());
        }

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
        load_anchor_rules(&desired)?;

        *applied = desired;
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        for ip in current_pool_aliases() {
            let _ = remove_lo0_alias(ip);
        }
        let _ = flush_anchor();
        let mut enabled_pf = self.enabled_pf.lock().unwrap();
        if *enabled_pf {
            let _ = disable_pf();
            *enabled_pf = false;
        }
        *self.applied.lock().unwrap() = Vec::new();
        Ok(())
    }
}

/// Pure add/remove diff between the currently-aliased and desired virtual-IP
/// sets — isolated from `current_pool_aliases`/`add_lo0_alias`/
/// `remove_lo0_alias` so it's unit-testable without shelling out.
fn diff_ips(
    current: &BTreeSet<Ipv4Addr>,
    desired: &BTreeSet<Ipv4Addr>,
) -> (Vec<Ipv4Addr>, Vec<Ipv4Addr>) {
    (
        desired.difference(current).copied().collect(),
        current.difference(desired).copied().collect(),
    )
}

/// Whether `ip` falls inside fghjd's exclusively-owned `10.222.0.0/16` pool
/// — the ownership predicate used both to prune stray aliases and to avoid
/// ever touching an `lo0` alias fghjd doesn't own.
fn in_pool(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 10 && octets[1] == 222
}

/// Parses `ifconfig lo0` output for `inet <ip> ...` lines whose address
/// falls inside fghjd's pool — isolated from the actual `ifconfig` call so
/// this parsing logic is unit-testable without shelling out.
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

/// Renders the anchor's full pf ruleset from scratch — a declarative
/// full-rewrite, same as `hosts_file::sync`'s "own the whole managed block"
/// approach, rather than incremental per-rule add/remove.
fn render_ruleset(routes: &[RouteSpec]) -> String {
    let mut out = String::new();
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

fn add_lo0_alias(ip: Ipv4Addr) -> Result<()> {
    run(Command::new("ifconfig").args(["lo0", "alias", &ip.to_string(), "up"]))
}

fn remove_lo0_alias(ip: Ipv4Addr) -> Result<()> {
    run(Command::new("ifconfig").args(["lo0", "-alias", &ip.to_string()]))
}

fn load_anchor_rules(routes: &[RouteSpec]) -> Result<()> {
    let ruleset = render_ruleset(routes);
    let mut child = Command::new("pfctl")
        .args(["-a", ANCHOR, "-f", "-"])
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
            "pfctl -a {ANCHOR} -f - failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn flush_anchor() -> Result<()> {
    run(Command::new("pfctl").args(["-a", ANCHOR, "-F", "all"]))
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
            "rdr pass on lo0 inet proto tcp from any to 10.222.1.1 port 9000 -> 127.0.0.1 port 54321\n"
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
}
