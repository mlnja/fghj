//! Everything `fghj uninstall` removes. The daemon is stood down before the
//! state directory goes, or it would re-mint and re-trust a CA.
//!
//! Unless `--keep-state` keeps all of it, nothing of fghj's CAs survives:
//! not trusted anywhere, not on disk, not in a running sidecar.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::{hosts_file, persistence, platform, raw_net};

/// For reinstalling.
#[derive(Debug, Default, Clone, Copy)]
pub struct Options {
    /// The root CA stays trusted, so a reinstall doesn't ask again.
    pub keep_ca: bool,
    /// All of `/var/lib/fghjd` stays, CA keys included.
    pub keep_state: bool,
    /// Only the workspace index and daemon settings stay; every CA file
    /// goes. What a package removal does: a reinstall mints a fresh CA and
    /// still knows the workspaces.
    pub keep_workspaces: bool,
}

/// What `keep_workspaces` leaves in the state directory. Neither holds key
/// material.
const WORKSPACE_FILES: &[&str] = &["workspaces.json", "daemon-state.json"];

/// What fghjd does for `remove_sidecars`, for when it can't.
const REMOVE_SIDECARS_BY_HAND: &str =
    "docker rm -f $(docker ps -aq --filter label=com.docker.compose.service=fghj-sidecar)";

#[derive(Debug, Default)]
pub struct Outcome {
    pub daemon_was_running: bool,
    pub certificates_removed: usize,
    pub sidecars_removed: usize,
    pub state_dir_removed: Option<PathBuf>,
    /// Workspaces whose `.fghj` database went with the state.
    pub workspace_dbs_removed: usize,
    /// Keeps going: remove what it can, say what it couldn't.
    pub warnings: Vec<String>,
}

/// Users' Docker resources are left alone: a volume may hold data they want,
/// and the `fghj-` prefix is a filter, not proof of ownership. Only fghj's
/// own sidecars go, which hold the signing CA's key.
///
/// `remove_sidecars` asks fghjd to remove them, `Ok(None)` if it isn't
/// running. It's called after `stop_daemon`, which only stands fghjd down:
/// the process and its control socket stay up.
pub fn purge(
    opts: Options,
    stop_daemon: impl FnOnce() -> Result<bool>,
    remove_sidecars: impl FnOnce() -> Result<Option<usize>>,
) -> Outcome {
    let mut outcome = Outcome::default();

    // Read first: it's what says whose browsers and workspaces fghj touched,
    // and it's gone once the state is.
    let workspaces: Vec<PathBuf> = persistence::load_index(&persistence::default_index_path())
        .into_values()
        .collect();

    match stop_daemon() {
        Ok(running) => outcome.daemon_was_running = running,
        Err(e) => outcome
            .warnings
            .push(format!("could not stop fghjd cleanly: {e:#}")),
    }

    // In case `fghjd` died without tearing these down. All idempotent.
    if let Err(e) = hosts_file::sync(&hosts_file::hosts_path(), &[]) {
        outcome
            .warnings
            .push(format!("could not clear the /etc/hosts block: {e:#}"));
    }
    let platform = platform::current();
    platform.dns.clear();
    if let Err(e) = raw_net::clear() {
        outcome
            .warnings
            .push(format!("could not clear the raw-zone listeners: {e:#}"));
    }
    if let Err(e) = platform.loopback.remove_alias(crate::web::proxy::PROXY_IP) {
        outcome.warnings.push(format!(
            "could not remove the {} {} alias: {e:#}",
            crate::web::proxy::PROXY_IP,
            platform.loopback.interface()
        ));
    }

    // Trust before disk: a trusted cert whose key we deleted is still a trusted
    // root.
    if !opts.keep_ca {
        outcome.certificates_removed = platform.trust.remove()
            + owners(&workspaces)
                .iter()
                .map(|owner| platform.browser_trust.remove(owner))
                .sum::<usize>();
    }

    if opts.keep_state {
        return outcome;
    }

    match remove_sidecars() {
        Ok(Some(n)) => outcome.sidecars_removed = n,
        Ok(None) => outcome.warnings.push(format!(
            "fghjd isn't running, so it couldn't remove fghj's sidecar containers; \
             remove any that are left with `{REMOVE_SIDECARS_BY_HAND}`"
        )),
        Err(e) => outcome
            .warnings
            .push(format!("could not remove fghj's sidecar containers: {e:#}")),
    }

    let root = persistence::fghjd_root();
    let removed = if opts.keep_workspaces {
        remove_state_except(&root, WORKSPACE_FILES)
    } else {
        outcome.workspace_dbs_removed = workspaces
            .iter()
            .filter(|ws| remove_workspace_db(ws))
            .count();
        remove_state_dir(&root)
    };
    match removed {
        Ok(true) => outcome.state_dir_removed = Some(root),
        Ok(false) => {}
        Err(e) => outcome
            .warnings
            .push(format!("could not remove {}: {e:#}", root.display())),
    }

    outcome
}

/// Every wired workspace's owner, and under `sudo` whoever ran it: the people
/// whose browsers fghj filled. One each.
fn owners(workspaces: &[PathBuf]) -> Vec<persistence::WorkspaceOwner> {
    let mut owners: Vec<persistence::WorkspaceOwner> = Vec::new();
    for owner in workspaces
        .iter()
        .filter_map(|ws| persistence::WorkspaceOwner::of_folder(ws))
        .chain(invoking_user())
    {
        if !owners.iter().any(|o| o.uid == owner.uid) {
            owners.push(owner);
        }
    }
    owners
}

/// Under `sudo`, as `fghj uninstall` and `apt` run, whoever ran it.
fn invoking_user() -> Option<persistence::WorkspaceOwner> {
    let uid: u32 = std::env::var("SUDO_UID").ok()?.parse().ok()?;
    let gid: u32 = std::env::var("SUDO_GID").ok()?.parse().ok()?;
    Some(persistence::WorkspaceOwner {
        uid,
        gid,
        home: persistence::workspace_owner::home_dir_of(uid)?,
        ssh_auth_sock: None,
    })
}

fn remove_state_dir(root: &Path) -> Result<bool> {
    if !root.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(root)
        .with_context(|| format!("failed to remove {}", root.display()))?;
    Ok(true)
}

/// Everything in `root` but `keep`, so no CA file or key is left beside them.
fn remove_state_except(root: &Path, keep: &[&str]) -> Result<bool> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(false);
    };
    for entry in entries.flatten() {
        if keep.iter().any(|k| entry.file_name() == *k) {
            continue;
        }
        let path = entry.path();
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed.with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(true)
}

/// fghj's own database in `workspace` (`.fghj/fghj.db` and SQLite's files
/// beside it), and the folder if that leaves it empty. Nothing else in the
/// workspace is fghj's to delete.
fn remove_workspace_db(workspace: &Path) -> bool {
    let dir = workspace.join(".fghj");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    let mut removed = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("fghj.db") {
            removed |= std::fs::remove_file(entry.path()).is_ok();
        }
    }
    let _ = std::fs::remove_dir(&dir);
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_an_absent_state_dir_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-existed");
        assert!(!remove_state_dir(&missing).unwrap());
    }

    #[test]
    fn removing_a_populated_state_dir_takes_the_whole_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("fghjd");
        std::fs::create_dir_all(root.join("ca")).unwrap();
        std::fs::write(root.join("ca").join("ca-key.pem"), "secret").unwrap();
        std::fs::write(root.join("workspaces.json"), "[]").unwrap();

        assert!(remove_state_dir(&root).unwrap());
        assert!(!root.exists());
    }

    #[test]
    fn keeping_the_workspaces_leaves_no_ca_material() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for sub in ["ca", "certs", "sidecar-ca", "runs/net"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        std::fs::write(root.join("ca/ca-key.pem"), "secret").unwrap();
        std::fs::write(root.join("sidecar-ca/signing-key.pem"), "secret").unwrap();
        std::fs::write(root.join("certs/cert.pem"), "public").unwrap();
        std::fs::write(root.join("workspaces.json"), "{}").unwrap();
        std::fs::write(root.join("daemon-state.json"), "{}").unwrap();

        assert!(remove_state_except(root, WORKSPACE_FILES).unwrap());
        let mut left: Vec<String> = std::fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["daemon-state.json", "workspaces.json"]);
    }

    #[test]
    fn only_fghjs_own_database_leaves_a_workspace() {
        let ws = tempfile::tempdir().unwrap();
        let fghj = ws.path().join(".fghj");
        std::fs::create_dir_all(&fghj).unwrap();
        for f in ["fghj.db", "fghj.db-wal", "fghj.db-shm"] {
            std::fs::write(fghj.join(f), "").unwrap();
        }
        std::fs::create_dir_all(ws.path().join("api")).unwrap();

        assert!(remove_workspace_db(ws.path()));
        assert!(!fghj.exists());
        assert!(ws.path().join("api").exists());

        // Something of the user's in `.fghj` keeps the folder.
        std::fs::create_dir_all(&fghj).unwrap();
        std::fs::write(fghj.join("fghj.db"), "").unwrap();
        std::fs::write(fghj.join("notes.txt"), "mine").unwrap();
        assert!(remove_workspace_db(ws.path()));
        assert!(fghj.join("notes.txt").exists());
    }

    #[test]
    fn a_failure_to_stop_the_daemon_does_not_abort_the_rest() {
        let opts = Options {
            keep_ca: true,
            keep_state: true,
            ..Options::default()
        };
        let outcome = purge(opts, || anyhow::bail!("socket refused"), || Ok(Some(0)));
        assert!(!outcome.daemon_was_running);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("socket refused")),
            "the stop failure must be reported, not swallowed: {:?}",
            outcome.warnings
        );
    }

    #[test]
    fn keep_flags_suppress_both_destructive_steps() {
        let opts = Options {
            keep_ca: true,
            keep_state: true,
            ..Options::default()
        };
        let outcome = purge(opts, || Ok(false), || Ok(Some(0)));
        assert_eq!(outcome.certificates_removed, 0);
        assert_eq!(outcome.sidecars_removed, 0);
        assert_eq!(outcome.state_dir_removed, None);
    }
}
