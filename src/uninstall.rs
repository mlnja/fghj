//! Everything `fghj uninstall` removes, and the order it has to happen in.
//!
//! Two of fghj's artifacts outlive `brew uninstall` and neither is
//! discoverable: a trusted root in the **System keychain**, and the daemon's
//! state directory under `/var/lib/fghjd` — which holds that CA's private
//! key. Homebrew's caveats used to just print the two `sudo` commands and
//! hope; this module runs them, so "I removed fghj" doesn't quietly leave a
//! trusted CA behind.
//!
//! Ordering is the only subtle part. The daemon has to be stood down
//! *before* the state directory goes, because a live `fghjd` would keep
//! writing to it and would re-mint and re-trust a CA on its next reconcile —
//! undoing the keychain deletion that just happened. `purge` therefore
//! unwinds system config first, then trust, then disk.
//!
//! Docker resources are deliberately **not** touched — see [`purge`].

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::web::trust;
use crate::{dns, hosts_file, persistence, raw_net};

/// What to leave behind. Both default to `false` (remove everything); the
/// flags exist for the "reinstalling, not leaving" case, where re-trusting a
/// freshly minted CA costs an interactive Authorization Services prompt and
/// re-wiring every workspace costs real work.
#[derive(Debug, Default, Clone, Copy)]
pub struct Options {
    pub keep_ca: bool,
    pub keep_state: bool,
}

/// What actually happened, for the caller to report. Counts rather than
/// booleans because "removed 0 certificates" and "removed 3" are different
/// facts to a user who just asked for the CA to be gone.
#[derive(Debug, Default)]
pub struct Outcome {
    pub daemon_was_running: bool,
    pub certificates_removed: usize,
    pub state_dir_removed: Option<PathBuf>,
    /// Non-fatal problems. Uninstall keeps going after these on purpose: a
    /// half-uninstall that stops at the first error is worse than one that
    /// removes what it can and says what it couldn't.
    pub warnings: Vec<String>,
}

/// Removes every fghj artifact except Docker resources and the binaries
/// themselves.
///
/// `stop_daemon` is injected rather than called directly so this stays
/// testable without a live `fghjd`: `main` passes a closure that POSTs to
/// the control socket, tests pass one that records the call. It returns
/// whether a daemon was actually there to stop.
///
/// Docker containers, networks, volumes and images are left alone, and that
/// is not an oversight. A named volume is *designed* to outlive its
/// containers — it can hold a database the user still wants — and fghj identifies
/// its Docker resources by an `fghj-` name prefix, which is a filter, not
/// proof of ownership. Deleting data on a name match during an uninstall is
/// not a call this command gets to make; it reports them instead.
pub fn purge(opts: Options, stop_daemon: impl FnOnce() -> Result<bool>) -> Outcome {
    let mut outcome = Outcome::default();

    match stop_daemon() {
        Ok(running) => outcome.daemon_was_running = running,
        Err(e) => outcome
            .warnings
            .push(format!("could not stop fghjd cleanly: {e:#}")),
    }

    // Belt and braces after the stop: if `fghjd` was killed uncleanly at
    // some point it never ran its own teardown, so the managed `/etc/hosts`
    // block, the resolver files, the `lo0` aliases and the pf anchor can all
    // still be installed with no daemon alive to unwind them. Every one of
    // these is idempotent and a no-op when there's nothing to remove.
    if let Err(e) = hosts_file::sync(&hosts_file::hosts_path(), &[]) {
        outcome
            .warnings
            .push(format!("could not clear the /etc/hosts block: {e:#}"));
    }
    dns::clear_os_resolver_config();
    if let Err(e) = raw_net::clear() {
        outcome
            .warnings
            .push(format!("could not clear pf rules and lo0 aliases: {e:#}"));
    }
    if let Err(e) = raw_net::remove_loopback_alias(crate::web::proxy::PROXY_IP) {
        outcome.warnings.push(format!(
            "could not remove the {} lo0 alias: {e:#}",
            crate::web::proxy::PROXY_IP
        ));
    }

    // Trust before disk: the private key under `ca/` is worthless once the
    // certificate is no longer trusted, but a trusted certificate whose key
    // we deleted is still a trusted root.
    if !opts.keep_ca {
        outcome.certificates_removed = trust::remove();
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

/// `Ok(false)` when there was nothing there — uninstalling twice, or
/// uninstalling something that never started, is not an error.
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

    /// The guarantee that makes `purge` safe to run on a machine whose
    /// daemon has already been killed: a failure to stop it is recorded and
    /// stepped over, not propagated, so the CA and state removal still run.
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

    /// `--keep-ca` and `--keep-state` are the whole reason `Options` exists;
    /// if either were ignored, a reinstall-in-place would silently throw
    /// away the trusted CA and every wired workspace.
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
