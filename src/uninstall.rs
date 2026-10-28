//! Everything `fghj uninstall` removes. The daemon is stood down before the
//! state directory goes, or it would re-mint and re-trust a CA.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::{hosts_file, persistence, platform, raw_net};

/// For reinstalling: keeps the trusted CA and wired workspaces.
#[derive(Debug, Default, Clone, Copy)]
pub struct Options {
    pub keep_ca: bool,
    pub keep_state: bool,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub daemon_was_running: bool,
    pub certificates_removed: usize,
    pub state_dir_removed: Option<PathBuf>,
    /// Keeps going: remove what it can, say what it couldn't.
    pub warnings: Vec<String>,
}

/// Docker resources are left alone: a volume may hold data the user wants, and
/// the `fghj-` prefix is a filter, not proof of ownership.
pub fn purge(opts: Options, stop_daemon: impl FnOnce() -> Result<bool>) -> Outcome {
    let mut outcome = Outcome::default();

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
            + invoking_user().map_or(0, |user| platform.browser_trust.remove(&user));
    }

    if !opts.keep_state {
        let root = persistence::fghjd_root();
        match remove_state_dir(&root) {
            Ok(true) => outcome.state_dir_removed = Some(root),
            Ok(false) => {}
            Err(e) => outcome
                .warnings
                .push(format!("could not remove {}: {e:#}", root.display())),
        }
    }

    outcome
}

/// Under `sudo`, as `fghj uninstall` runs, whoever ran it: their browsers are
/// the ones fghj filled.
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

fn remove_state_dir(root: &std::path::Path) -> Result<bool> {
    if !root.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(root)
        .with_context(|| format!("failed to remove {}", root.display()))?;
    Ok(true)
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
    fn a_failure_to_stop_the_daemon_does_not_abort_the_rest() {
        let opts = Options {
            keep_ca: true,
            keep_state: true,
        };
        let outcome = purge(opts, || anyhow::bail!("socket refused"));
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
        };
        let outcome = purge(opts, || Ok(false));
        assert_eq!(outcome.certificates_removed, 0);
        assert_eq!(outcome.state_dir_removed, None);
    }
}
