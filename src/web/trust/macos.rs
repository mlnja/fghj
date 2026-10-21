use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::web::ca::COMMON_NAME;

/// Machine-wide: every user's browser must trust it.
pub(super) const SYSTEM_KEYCHAIN: &str = "/Library/Keychains/System.keychain";

/// Loops: `delete-certificate` removes one match per call, and repeated installs
/// leave several CAs. A non-zero exit means none are left.
pub(super) fn remove_trust() -> usize {
    let mut removed = 0;
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

/// `verify-cert` evaluates trust without prompting, so this is safe to call
/// anytime.
pub(super) fn is_trusted(ca_cert_path: &Path) -> bool {
    Command::new("security")
        .args(["verify-cert", "-c"])
        .arg(ca_cert_path)
        .args(["-k", SYSTEM_KEYCHAIN])
        // `output`, not `status`, to keep its chatter out of the log.
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Checks first, since `add-trusted-cert` always prompts. Runs on every start,
/// so revoked trust self-heals.
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
        // A launchd system daemon has no session to prompt in, so under
        // `brew services` this always fails; the CLI, in a terminal, can.
        anyhow::bail!(
            "`security add-trusted-cert` failed for {cert}\n\
             macOS asks for a password before trusting a root, and only a process in \
             your login session can ask: fghjd under `brew services` or ssh can't. \
             From a terminal, run `fghj daemon restart`, or:\n\
             \n\
             \x20   security add-trusted-cert -d -r trustRoot -k {keychain} {cert}",
            cert = ca_cert_path.display(),
            keychain = SYSTEM_KEYCHAIN,
        );
    }
    println!("installed the fghj local CA into the System trust store");
    Ok(())
}
