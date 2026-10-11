use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::web::proxy::PROXY_IP;

/// Exported so `uninstall` can strip the block without fghjd running.
pub const BEGIN_MARKER: &str = "# fghj-managed-begin";
pub const END_MARKER: &str = "# fghj-managed-end";

pub fn hosts_path() -> PathBuf {
    PathBuf::from("/etc/hosts")
}

/// `hosts` comes from running containers only; sorted and deduped so
/// identical sets don't rewrite.
pub fn sync(path: &Path, hosts: &[String]) -> Result<()> {
    let mut hosts: Vec<&str> = hosts.iter().map(String::as_str).collect();
    hosts.sort_unstable();
    hosts.dedup();

    let existing = fs::read_to_string(path).unwrap_or_default();
    let mut out = String::new();
    let mut in_block = false;
    for line in existing.lines() {
        match line.trim() {
            _ if line.trim() == BEGIN_MARKER => in_block = true,
            _ if line.trim() == END_MARKER => in_block = false,
            _ if in_block => {}
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }

    if !hosts.is_empty() {
        out.push_str(BEGIN_MARKER);
        out.push('\n');
        for host in hosts {
            // Same proxy as `*.fghj.internal`, so the same address.
            out.push_str(&PROXY_IP.to_string());
            out.push(' ');
            out.push_str(host);
            out.push('\n');
        }
        out.push_str(END_MARKER);
        out.push('\n');
    }

    if out != existing {
        fs::write(path, &out).with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

/// Reads the file, so it reflects what's installed, not what's desired.
pub fn managed_hosts(path: &Path) -> Vec<String> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let mut in_block = false;
    let mut hosts = Vec::new();
    for line in existing.lines() {
        match line.trim() {
            _ if line.trim() == BEGIN_MARKER => in_block = true,
            _ if line.trim() == END_MARKER => in_block = false,
            trimmed if in_block => {
                // `127.0.0.1` too, as older versions wrote, until the next `sync`.
                if let Some(host) = trimmed
                    .strip_prefix(&format!("{PROXY_IP} "))
                    .or_else(|| trimmed.strip_prefix("127.0.0.1 "))
                {
                    hosts.push(host.to_string());
                }
            }
            _ => {}
        }
    }
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_adds_and_removes_the_managed_block_without_touching_other_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hosts");
        fs::write(&path, "127.0.0.1 localhost\n::1 localhost\n").unwrap();

        sync(
            &path,
            &["aikido.local".to_string(), "demo.example.com".to_string()],
        )
        .unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("127.0.0.1 localhost\n::1 localhost\n"));
        assert!(contents.contains(&format!(
            "{BEGIN_MARKER}\n{PROXY_IP} aikido.local\n{PROXY_IP} demo.example.com\n{END_MARKER}\n"
        )));

        sync(&path, &[]).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "127.0.0.1 localhost\n::1 localhost\n"
        );
    }

    #[test]
    fn sync_is_idempotent_and_sorts_deduplicates_hosts() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hosts");
        fs::write(&path, "").unwrap();

        sync(
            &path,
            &[
                "b.local".to_string(),
                "a.local".to_string(),
                "a.local".to_string(),
            ],
        )
        .unwrap();
        let first = fs::read_to_string(&path).unwrap();
        assert_eq!(
            first,
            format!("{BEGIN_MARKER}\n{PROXY_IP} a.local\n{PROXY_IP} b.local\n{END_MARKER}\n")
        );

        sync(&path, &["a.local".to_string(), "b.local".to_string()]).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
    }

    #[test]
    fn sync_replaces_a_stale_block_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hosts");
        fs::write(
            &path,
            format!("before\n{BEGIN_MARKER}\n127.0.0.1 stale.local\n{END_MARKER}\nafter\n"),
        )
        .unwrap();

        sync(&path, &["fresh.local".to_string()]).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("before\nafter\n{BEGIN_MARKER}\n{PROXY_IP} fresh.local\n{END_MARKER}\n")
        );
    }

    /// Not `127.0.0.1`, which would shadow the developer's own services.
    #[test]
    fn managed_entries_point_at_the_proxy_address_not_plain_loopback() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hosts");
        fs::write(&path, "").unwrap();

        sync(&path, &["shop.local".to_string()]).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains(&format!("{PROXY_IP} shop.local")),
            "{contents}"
        );
        assert!(
            !contents.contains("127.0.0.1 shop.local"),
            "managed entries must not claim plain loopback: {contents}"
        );
    }

    #[test]
    fn managed_hosts_still_reads_a_block_written_by_an_older_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hosts");
        fs::write(
            &path,
            format!("{BEGIN_MARKER}\n127.0.0.1 legacy.local\n{END_MARKER}\n"),
        )
        .unwrap();

        assert_eq!(managed_hosts(&path), vec!["legacy.local".to_string()]);
    }

    #[test]
    fn managed_hosts_reads_back_exactly_what_sync_wrote() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hosts");
        fs::write(&path, "127.0.0.1 localhost\n::1 localhost\n").unwrap();

        assert!(managed_hosts(&path).is_empty());

        sync(&path, &["b.local".to_string(), "a.local".to_string()]).unwrap();
        assert_eq!(managed_hosts(&path), vec!["a.local", "b.local"]);
    }
}
