//! macOS `pf`/`ifconfig` implementation of [`super::RawNetBackend`].
//!
//! Two earlier designs were tried and abandoned this cycle:
//!
//! 1. **Nested anchor under `com.apple/*`** (`pfctl -a com.apple/fghjd -f -`),
//!    piggybacking on stock macOS's `rdr-anchor "com.apple/*"` wildcard hook.
//!    Proved unreliable in practice: `rdr` rules loaded two levels under that
//!    wildcard simply never fired — confirmed even on a completely fresh boot
//!    (empty state table, pf enabled under 5 minutes). Neither re-issuing the
//!    anchor's rules nor a full `pfctl -d && pfctl -e` cycle nor a real reboot
//!    fixed it.
//! 2. **Owning the top-level ruleset directly**: every tick, read
//!    `/etc/pf.conf` fresh off disk, splice in our own `rdr` lines, and
//!    `pfctl -f -` the result. This "fixed" (1) but caused a much worse
//!    regression: on a real machine, Docker Desktop injects its own NAT/rdr
//!    rules directly into the *live* kernel ruleset without ever writing them
//!    to `/etc/pf.conf` (the same "invisible on disk, present live" pattern
//!    already observed with `com.apple.internet-sharing`). Since our reload
//!    only knew about what's on disk, every tick silently erased Docker's
//!    live-only rules within a second of `fghjd` starting, breaking *all*
//!    `127.0.0.1:<published-port>` connectivity — a direct violation of the
//!    one hard rule for this feature: fghjd must never be the one who breaks
//!    someone else's networking, even if someone else reloading pf is
//!    allowed to transiently break *us*.
//!
//! This backend instead owns a **dedicated top-level named anchor**
//! (`fghjd`, not nested under `com.apple`): a single idempotent one-time edit
//! adds a bare `rdr-anchor "fghjd"` hook line to `/etc/pf.conf` (see
//! `install_anchor_hook`) — the same, ordinary way most third-party pf-based
//! tools (Little Snitch and friends) hook in. After that, every reconcile
//! tick only ever runs `pfctl -a fghjd -f -`, which replaces *our own
//! anchor's* content and nothing else — it cannot see or touch whatever
//! Docker, `com.apple.internet-sharing`, or anything else has injected into
//! the top-level ruleset or its own anchors, live or on disk. The one-time
//! hook line is removed again on a clean `clear()` (see
//! `remove_anchor_hook`), but is otherwise harmless to leave behind if fghjd
//! is killed — an anchor hook with nothing loaded into it is a no-op, exactly
//! like the dangling `com.apple/*` hooks already present by default.
//!
//! Every command-running function here is a thin wrapper around pure,
//! independently-testable logic (`diff_ips`, `render_ruleset`,
//! `parse_pool_aliases`, `in_pool`, `strip_managed_block`,
//! `insert_after_translation_hooks`, `install_anchor_hook`) — the actual
//! `Command`/filesystem calls are the only part that needs macOS and root to
//! exercise for real.

use std::collections::BTreeSet;
use std::io::Write;
use std::net::Ipv4Addr;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use super::{RawNetBackend, RouteSpec};

const PF_CONF_PATH: &str = "/etc/pf.conf";
const ANCHOR_NAME: &str = "fghjd";
/// Bracket the one-time anchor-hook line spliced into `/etc/pf.conf` so it
/// can be found and stripped again — same "own a managed block" pattern
/// `hosts_file::sync` uses for `/etc/hosts`.
const HOOK_MARKER_BEGIN: &str =
    "# --- fghjd raw-net anchor hook (managed; do not edit — see raw_net::macos) ---";
const HOOK_MARKER_END: &str = "# --- end fghjd raw-net anchor hook ---";
const ANCHOR_HOOK_LINE: &str = "rdr-anchor \"fghjd\"";

pub struct MacosPfBackend {
    /// Last-applied route set — used for `status()` and to diff which `lo0`
    /// aliases actually need adding/removing.
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

/// Renders our anchor's full content from scratch — a declarative
/// full-rewrite of *just our own anchor*, same as `hosts_file::sync`'s "own
/// the whole managed block" approach, rather than incremental per-rule
/// add/remove. Safe to fully rewrite every tick because `fghjd` is this
/// anchor's sole owner by construction (nothing else loads into an anchor
/// named `fghjd`).
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

/// Removes a previously-spliced managed block (if any) from `pf.conf`
/// content — used both to clear the way for a fresh splice and, on its own,
/// to produce the "restore pf.conf to its unmodified state" content.
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

/// Inserts `insert_lines` right after the last existing `nat-anchor`/
/// `rdr-anchor` line: pf requires `translation` rules (nat/rdr) to precede
/// `filtering` rules (plain `anchor`, `block`, `pass`) in the ruleset text,
/// so anywhere in the translation section works; falls back to just before
/// the first filter-type `anchor` line, or the end of the file, if a
/// customized `pf.conf` doesn't have the expected hooks.
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

/// Splices fghjd's one-time anchor hook line into a copy of `pf.conf`'s
/// content — idempotent: stripping then reinserting an already-installed
/// hook reproduces the same content, so callers can compare before/after and
/// skip writing the file back when nothing changed.
fn install_anchor_hook(conf: &str) -> String {
    let cleaned = strip_managed_block(conf);
    insert_after_translation_hooks(
        &cleaned,
        &[HOOK_MARKER_BEGIN, ANCHOR_HOOK_LINE, HOOK_MARKER_END],
    )
}

/// Ensures the `rdr-anchor "fghjd"` hook exists in `/etc/pf.conf`, writing
/// the file only the first time (or if something else stripped it since) —
/// after that, every tick's `install_anchor_hook` output is byte-identical
/// to what's already on disk, so no write happens.
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

/// Removes fghjd's anchor hook from `/etc/pf.conf`, restoring it to its
/// pristine state — called on a clean `clear()` only; if fghjd is killed
/// instead, the leftover hook is a harmless no-op anchor point.
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

/// Reloads the top-level ruleset straight from `/etc/pf.conf` on disk — used
/// only right after *we* just edited that file (installing/removing our own
/// hook line), so pf picks up the new hook point. This is the one place this
/// backend still touches the top-level ruleset, and only ever mirrors
/// whatever is already on disk (including our own just-written edit), never
/// a synthesized/spliced-in-memory version — so it can't clobber any
/// live-only state another tool injected, since it doesn't touch anything
/// beyond what's already persisted.
fn reload_top_level_ruleset_from_disk() -> Result<()> {
    run(Command::new("pfctl").args(["-f", PF_CONF_PATH]))
}

/// Replaces fghjd's own named anchor's content — never touches the
/// top-level ruleset or any other anchor, so it cannot clobber anything else
/// on the system, live or on disk.
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
