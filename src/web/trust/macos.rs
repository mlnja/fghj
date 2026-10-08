//! macOS half of [`super`]: the System keychain, driven through the
//! `security` CLI.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::web::ca::COMMON_NAME;

/// Machine-wide, not per-user: the CA has to be trusted for every browser
/// and every user on the box, and `fghjd` already runs as root.
pub(super) const SYSTEM_KEYCHAIN: &str = "/Library/Keychains/System.keychain";

/// The inverse of [`install_trust`] — deletes every System-keychain
/// certificate named [`COMMON_NAME`], returning how many it removed.
///
/// Loops rather than deleting once because `security delete-certificate`
/// removes a single match per invocation, and a machine that has run
/// several `fghjd` installs can hold several: deleting
/// `/var/lib/fghjd/ca/` makes the next start mint a *new* CA and trust it
/// too, leaving the old one behind. Uninstalling has to clear all of them
/// or it leaves trusted roots whose private keys the user thinks they
/// deleted.
///
/// A non-zero exit means "no certificate by that name", which is the
/// success condition here, not an error — so the loop ends on the first
/// failure and reports the count rather than propagating it.
pub(super) fn remove_trust() -> usize {
    let mut removed = 0;
    // Bounded so a `security` that somehow always succeeds can't spin
    // forever; far above any plausible number of stale fghj CAs.
    while removed < 32 {
        let ok = Command::new("security")
            .args(["delete-certificate", "-c", COMMON_NAME, SYSTEM_KEYCHAIN])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            break;
        }
        removed += 1;
    }
    removed
}

/// Whether `ca_cert_path` is already trusted as a root in the macOS System
/// keychain. `security verify-cert` is a trust *evaluation*, not a trust
/// *modification* — unlike `add-trusted-cert` it never triggers an
/// interactive Authorization Services prompt, so this is safe (and cheap) to
/// call unconditionally. That promptlessness is also why `doctor` can report trust as
/// a read-only check rather than having to attempt the install to find out.
pub(super) fn is_trusted(ca_cert_path: &Path) -> bool {
    Command::new("security")
        .args(["verify-cert", "-c"])
        .arg(ca_cert_path)
        .args(["-k", SYSTEM_KEYCHAIN])
        // `output` rather than `status` purely to capture the subprocess's
        // own chatter: `verify-cert` prints "certificate verification
        // successful" on every call, and `doctor` calls this on demand, so
        // inheriting stdout would scatter that line through `fghjd`'s log.
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Ensures `ca_cert_path` is trusted as a root in the macOS System keychain,
/// so certificates this CA issues are accepted by the browser without a
/// warning. Safe to call on every `fghjd` start, same as
/// `dns::install_os_resolver_config`: it first checks whether the cert is
/// already trusted (a promptless read) and only falls through to the actual
/// `add-trusted-cert` write — which always triggers an interactive
/// Authorization Services password prompt on macOS, root privilege
/// notwithstanding — when it genuinely isn't. This also means trust that
/// goes missing after the fact (e.g. a user manually revokes it in Keychain
/// Access, or the keychain gets reset) self-heals on the next `fghjd`
/// restart instead of silently staying broken.
pub(super) fn install_trust(ca_cert_path: &Path) -> Result<()> {
    if is_trusted(ca_cert_path) {
        return Ok(());
    }

    let status = Command::new("security")
        .args([
            "add-trusted-cert",
            "-d",
            "-r",
            "trustRoot",
            "-k",
            SYSTEM_KEYCHAIN,
        ])
        .arg(ca_cert_path)
        .status()
        .context("failed to run `security add-trusted-cert`")?;
    if !status.success() {
        // Nearly always one specific thing: `SecTrustSettingsSetTrustSettings:
        // The authorization was denied since no user interaction was
        // possible.` Modifying System trust needs an Authorization Services
        // prompt, and a launchd *system* daemon has no session to show one in
        // — root does not bypass that gate. So the failure is not transient,
        // and with `KeepAlive` set the daemon would otherwise crash-loop on it
        // forever, logging a bare "failed" with nothing to act on. Hand over
        // the exact command instead: run from a terminal it can prompt, and
        // the check at the top of this function makes every later start a
        // no-op.
        anyhow::bail!(
            "`security add-trusted-cert` failed for {cert}\n\
             If this says the authorization was denied because no user interaction was \
             possible, fghjd is running without a session to prompt in (a launchd \
             LaunchDaemon, `brew services`, ssh). Install the trust once from a terminal:\n\
             \n\
             \x20   sudo security add-trusted-cert -d -r trustRoot -k {keychain} {cert}\n\
             \n\
             then start fghjd again.",
            cert = ca_cert_path.display(),
            keychain = SYSTEM_KEYCHAIN,
        );
    }
    println!("fghjd: installed the fghj local CA into the System trust store");
    Ok(())
}
