//! macOS half of [`super::install_os_resolver_config`]: one file per zone
//! under `/etc/resolver`, the per-domain mechanism macOS's system resolver
//! reads. Every function takes the directory as a parameter so tests can
//! point it at a tempdir instead of the real root-owned path.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::daemon_log;

pub(super) const RESOLVER_DIR: &str = "/etc/resolver";

/// Parses the port back out of a resolver file's content if it matches
/// fghjd's own template — the single source of truth for "is this a file
/// fghjd itself wrote, and if so at what port," used both to recognize
/// fghjd-authored files (`is_fghjd_resolver_content`) and to read the port
/// back for the telemetry status endpoint (`managed_resolver_zones`).
fn parse_fghjd_resolver_port(content: &str) -> Option<u16> {
    content
        .strip_prefix("nameserver 127.0.0.1\nport ")
        .and_then(|rest| rest.strip_suffix('\n'))
        .and_then(|port| port.parse::<u16>().ok())
}

/// Recognizing exactly this shape (regardless of port) is how
/// `sync_resolver`/`clear_resolver` tell "a file fghjd itself
/// created" apart from a resolver file some other tool placed, without
/// needing a separate tracking manifest.
fn is_fghjd_resolver_content(content: &str) -> bool {
    parse_fghjd_resolver_port(content).is_some()
}

/// Reads back which zones are currently routed to this DNS server and at
/// what port, by scanning `resolver_dir` for fghjd-authored files
/// (`parse_fghjd_resolver_port`) — the on-disk state `sync_resolver`
/// last wrote is the source of truth, so this re-parses it rather than
/// tracking a separate list. Backs the telemetry drawer's network-status tab
/// (`daemon/`'s `/daemon/net-status`).
pub(super) fn managed_resolver_zones(resolver_dir: &Path) -> Vec<(String, u16)> {
    let Ok(entries) = fs::read_dir(resolver_dir) else {
        return Vec::new();
    };
    let mut zones: Vec<(String, u16)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?.to_string();
            let content = fs::read_to_string(&path).ok()?;
            let port = parse_fghjd_resolver_port(&content)?;
            Some((name, port))
        })
        .collect();
    zones.sort();
    zones
}

/// Writes (or refreshes) one `resolver_dir/<zone>` file per entry in `zones`
/// — idempotently, same as before — then removes any *other* file in that
/// directory whose content matches fghjd's own template
/// (`is_fghjd_resolver_content`) but whose zone isn't in `zones` anymore,
/// e.g. a `wildcard_hosts` suffix whose owning container just stopped. A
/// resolver file some other tool created is never touched, since its
/// content won't match the template.
pub(super) fn sync_resolver(resolver_dir: &Path, port: u16, zones: &[&str]) -> Result<()> {
    fs::create_dir_all(resolver_dir)
        .with_context(|| format!("failed to create {}", resolver_dir.display()))?;
    let desired = format!("nameserver 127.0.0.1\nport {port}\n");

    for zone in zones {
        let path = resolver_dir.join(zone);
        if fs::read_to_string(&path).ok().as_deref() != Some(desired.as_str()) {
            fs::write(&path, &desired)
                .with_context(|| format!("failed to write {}", path.display()))?;
            // The path already ends in the zone, so naming the zone again
            // spent 60-odd columns restating it — on the one line in this
            // log that is already the longest, and that repeats on every
            // reconcile which finds the file changed.
            daemon_log::info(format!(
                "fghjd: wrote {} — lookups for that zone now route to this DNS server",
                path.display()
            ));
        }
    }

    if let Ok(entries) = fs::read_dir(resolver_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if zones.contains(&name) {
                continue;
            }
            if fs::read_to_string(&path)
                .ok()
                .is_some_and(|c| is_fghjd_resolver_content(&c))
            {
                let _ = fs::remove_file(&path);
            }
        }
    }
    Ok(())
}

/// Removes every fghjd-authored resolver file in `resolver_dir`
/// unconditionally (content-based, same rule as `sync_resolver`) —
/// the full-teardown counterpart used on `deactivate`.
pub(super) fn clear_resolver(resolver_dir: &Path) {
    let Ok(entries) = fs::read_dir(resolver_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if fs::read_to_string(&path)
            .ok()
            .is_some_and(|c| is_fghjd_resolver_content(&c))
        {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns::ZONE;

    #[test]
    fn sync_resolver_is_idempotent_and_writes_expected_content() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver_dir = tmp.path().join("resolver");

        sync_resolver(&resolver_dir, 54321, &[ZONE]).unwrap();
        let path = resolver_dir.join(ZONE);
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "nameserver 127.0.0.1\nport 54321\n");

        // Re-running must not error and must leave the file as-is.
        sync_resolver(&resolver_dir, 54321, &[ZONE]).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    }

    #[test]
    fn sync_resolver_writes_multiple_zones_and_prunes_stale_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver_dir = tmp.path().join("resolver");

        sync_resolver(&resolver_dir, 1234, &[ZONE, "myservice.local"]).unwrap();
        assert!(resolver_dir.join(ZONE).exists());
        assert!(resolver_dir.join("myservice.local").exists());

        // A foreign file (content some other tool wrote) must survive.
        let foreign = resolver_dir.join("example.com");
        fs::write(&foreign, "nameserver 8.8.8.8\n").unwrap();

        // The wildcard zone's owning container stopped — it drops out of
        // the wanted set and its file must be removed, but the foreign
        // file and the fixed zone's file must be untouched.
        sync_resolver(&resolver_dir, 1234, &[ZONE]).unwrap();
        assert!(resolver_dir.join(ZONE).exists());
        assert!(!resolver_dir.join("myservice.local").exists());
        assert_eq!(
            fs::read_to_string(&foreign).unwrap(),
            "nameserver 8.8.8.8\n"
        );
    }

    #[test]
    fn clear_resolver_removes_only_fghjd_authored_files() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver_dir = tmp.path().join("resolver");

        sync_resolver(&resolver_dir, 1234, &[ZONE, "myservice.local"]).unwrap();
        let foreign = resolver_dir.join("example.com");
        fs::write(&foreign, "nameserver 8.8.8.8\n").unwrap();

        clear_resolver(&resolver_dir);

        assert!(!resolver_dir.join(ZONE).exists());
        assert!(!resolver_dir.join("myservice.local").exists());
        assert!(foreign.exists());
    }

    #[test]
    fn managed_resolver_zones_reads_back_fghjd_authored_zones_only() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver_dir = tmp.path().join("resolver");

        sync_resolver(&resolver_dir, 5353, &[ZONE, "myservice.local"]).unwrap();
        fs::write(resolver_dir.join("example.com"), "nameserver 8.8.8.8\n").unwrap();

        let mut zones = managed_resolver_zones(&resolver_dir);
        zones.sort();
        let mut expected = vec![
            (ZONE.to_string(), 5353),
            ("myservice.local".to_string(), 5353),
        ];
        expected.sort();
        assert_eq!(zones, expected);
    }

    #[test]
    fn managed_resolver_zones_of_a_missing_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(managed_resolver_zones(&tmp.path().join("nonexistent")).is_empty());
    }
}
