use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::web::proxy::PROXY_IP;

/// Marks the block `sync` owns inside `/etc/hosts` — everything else in the
/// file (the user's own entries, macOS's default `127.0.0.1 localhost` line,
/// etc.) is left exactly as found. Exported so `main.rs`'s `daemon_stop`
/// (which runs unprivileged and shells out via `sudo` the same way it does
/// for the pidfile/resolver-file cleanup) can strip the block without fghjd
/// itself being alive to do it.
pub const BEGIN_MARKER: &str = "# fghj-managed-begin";
pub const END_MARKER: &str = "# fghj-managed-end";

pub fn hosts_path() -> PathBuf {
    PathBuf::from("/etc/hosts")
}

/// Rewrites the managed block inside `path` to contain exactly one
/// `<PROXY_IP> <host>` line per entry in `hosts` — every other line in the
/// file, including anything outside the markers, is preserved untouched.
/// Called with the full set of `additional_hosts` declared by every
/// currently-*running* container across every wired workspace (see
/// `effects::hosts::HostsEffect`), so a host stops being claimed here the
/// moment its container stops, same lifecycle as a `state::PortRoute`.
/// `hosts` need not be sorted or deduped — `sync` does
/// both, so repeated calls with the same logical set never produce a
/// spurious rewrite (mirrors `dns::sync_macos_resolver`'s idempotent
/// full-file rewrite).
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
            // These names are served by the same proxy as `*.fghj.internal`,
            // so they have to point at the same address it binds — see
            // `web::proxy::PROXY_IP` for why that isn't `127.0.0.1`.
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

/// Reads back exactly what `sync` last wrote into the managed block. The
/// on-disk file is the source of truth here — this re-parses it rather than
/// tracking a separate list — so it reflects what's actually installed, not
/// merely what was last computed as desired. Backs the telemetry drawer's
/// network-status tab (`daemon/`'s `/daemon/net-status`).
pub fn managed_hosts(path: &Path) -> Vec<String> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let mut in_block = false;
    let mut hosts = Vec::new();
    for line in existing.lines() {
        match line.trim() {
            _ if line.trim() == BEGIN_MARKER => in_block = true,
            _ if line.trim() == END_MARKER => in_block = false,
            trimmed if in_block => {
                // `127.0.0.1 ` is accepted as well as the current address so
                // that upgrading from a version that wrote loopback doesn't
                // under-report the block until the next `sync` rewrites it.
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

        // Removing every host must drop the block entirely, leaving the
        // original untouched lines exactly as they were.
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

        // Re-syncing with the same logical set (different input order) must
        // not touch the file's mtime-worthy content a second time.
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

    /// The whole point of the address change: an entry fghj claims must not
    /// be pinned to `127.0.0.1`, or it would shadow whatever the developer
    /// is already running there.
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

    /// Upgrading from a build that wrote `127.0.0.1` must not make the
    /// existing block invisible to the telemetry drawer in the window before
    /// the next `sync` rewrites it.
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
