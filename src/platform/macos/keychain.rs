//! macOS trust: the root CA in the System keychain, via `security`.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::platform::TrustStore;
use crate::web::ca::COMMON_NAME;

/// Machine-wide: every user's browser must trust it.
const SYSTEM_KEYCHAIN: &str = "/Library/Keychains/System.keychain";

pub(super) struct SystemKeychain;

impl TrustStore for SystemKeychain {
    fn install(&self, ca_cert_path: &Path) -> Result<()> {
        install_trust(ca_cert_path)
    }

    fn remove(&self) -> usize {
        remove_trust()
    }

    fn is_trusted(&self, ca_cert_path: &Path) -> Option<bool> {
        Some(is_trusted(ca_cert_path))
    }

    fn store_name(&self) -> &'static str {
        SYSTEM_KEYCHAIN
    }

    fn install_notice(&self) -> &'static str {
        "fghj's root CA isn't trusted yet — macOS will ask for your password to trust it"
    }

    fn untrusted_hint(&self) -> &'static str {
        "run `fghj doctor --fix` from a terminal — it asks macOS to trust the CA, \
         which shows a password dialog"
    }
}

/// Loops: `delete-certificate` removes one match per call, and repeated installs
/// leave several CAs. A non-zero exit means none are left.
fn remove_trust() -> usize {
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
fn is_trusted(ca_cert_path: &Path) -> bool {
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
fn install_trust(ca_cert_path: &Path) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_is_the_machine_wide_keychain() {
        assert_eq!(
            SystemKeychain.store_name(),
            "/Library/Keychains/System.keychain"
        );
    }
}
