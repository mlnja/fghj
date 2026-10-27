//! macOS split DNS: one file per zone under `/etc/resolver`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::daemon_log;
use crate::platform::{DnsRouting, OsRouting};

const RESOLVER_DIR: &str = "/etc/resolver";

pub(super) struct ResolverDir;

impl DnsRouting for ResolverDir {
    fn sync(&self, port: u16, zones: &[&str]) -> Result<()> {
        sync_resolver(Path::new(RESOLVER_DIR), port, zones)
    }

    fn clear(&self) {
        clear_resolver(Path::new(RESOLVER_DIR));
    }

    fn status(&self) -> Option<OsRouting> {
        Some(OsRouting {
            location: RESOLVER_DIR,
            zones: managed_resolver_zones(Path::new(RESOLVER_DIR)),
        })
    }

    fn footprint(&self) -> Option<&'static str> {
        Some("/etc/resolver/fghj*")
    }
}

fn parse_fghjd_resolver_port(content: &str) -> Option<u16> {
    content
        .strip_prefix("nameserver 127.0.0.1\nport ")
        .and_then(|rest| rest.strip_suffix('\n'))
        .and_then(|port| port.parse::<u16>().ok())
}

/// Recognizing fghjd's own template is how it tells its files from other
/// tools', with no manifest.
fn is_fghjd_resolver_content(content: &str) -> bool {
    parse_fghjd_resolver_port(content).is_some()
}

fn managed_resolver_zones(resolver_dir: &Path) -> Vec<(String, u16)> {
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

/// Also removes fghjd-authored files for zones no longer wanted; other tools'
/// files are never touched.
fn sync_resolver(resolver_dir: &Path, port: u16, zones: &[&str]) -> Result<()> {
    fs::create_dir_all(resolver_dir)
        .with_context(|| format!("failed to create {}", resolver_dir.display()))?;
    let desired = format!("nameserver 127.0.0.1\nport {port}\n");

    for zone in zones {
        let path = resolver_dir.join(zone);
        if fs::read_to_string(&path).ok().as_deref() != Some(desired.as_str()) {
            fs::write(&path, &desired)
                .with_context(|| format!("failed to write {}", path.display()))?;
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

fn clear_resolver(resolver_dir: &Path) {
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

        let foreign = resolver_dir.join("example.com");
        fs::write(&foreign, "nameserver 8.8.8.8\n").unwrap();

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
    fn status_reads_back_the_system_resolver_dir_sorted() {
        let routing = ResolverDir.status().expect("macOS has OS DNS routing");
        assert_eq!(routing.location, "/etc/resolver");
        assert!(routing.zones.is_sorted());
    }

    #[test]
    fn managed_resolver_zones_of_a_missing_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(managed_resolver_zones(&tmp.path().join("nonexistent")).is_empty());
    }
}
