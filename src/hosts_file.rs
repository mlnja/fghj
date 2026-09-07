use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

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
/// `127.0.0.1 <host>` line per entry in `hosts` — every other line in the
/// file, including anything outside the markers, is preserved untouched.
/// Called with the full set of `additional_hosts` declared by every
/// currently-*running* container across every wired workspace (see
/// `daemon::WorkspaceRegistry::active_additional_hosts`), so a host stops
/// being claimed here the moment its container stops, same lifecycle as a
/// `runs::PortRoute`. `hosts` need not be sorted or deduped — `sync` does
/// both, so repeated calls with the same logical set never produce a
/// spurious rewrite (mirrors `dns::install_macos_resolver`'s idempotent
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
            out.push_str("127.0.0.1 ");
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
            "{BEGIN_MARKER}\n127.0.0.1 aikido.local\n127.0.0.1 demo.example.com\n{END_MARKER}\n"
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
            format!("{BEGIN_MARKER}\n127.0.0.1 a.local\n127.0.0.1 b.local\n{END_MARKER}\n")
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
            format!("before\nafter\n{BEGIN_MARKER}\n127.0.0.1 fresh.local\n{END_MARKER}\n")
        );
    }
}
