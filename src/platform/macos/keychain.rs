//! macOS trust: the root CA in your login keychain, trusted for your user
//! only, via `security`.
//!
//! Not the System keychain: writing there needs root, and trusting a root
//! needs a password dialog, which only a process in your login session can
//! show. No single process has both, so the CLI does it, as you.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::persistence::WorkspaceOwner;
use crate::platform::TrustStore;
use crate::web::ca::COMMON_NAME;

/// Where fghjd ≤ 0.2.0 put the CA. Only cleaned up now.
const SYSTEM_KEYCHAIN: &str = "/Library/Keychains/System.keychain";

pub(super) struct LoginKeychain;

impl TrustStore for LoginKeychain {
    /// A per-user store, and macOS asks for a password before trusting a root;
    /// a launchd daemon is neither that user nor has a session to ask in.
    fn installed_by_user(&self) -> bool {
        true
    }

    fn install(&self, ca_cert_path: &Path) -> Result<()> {
        install_trust(ca_cert_path)
    }

    fn remove(&self, users: &[WorkspaceOwner]) -> usize {
        let legacy = delete_all(Command::new("security"), Path::new(SYSTEM_KEYCHAIN), false);
        legacy
            + users
                .iter()
                .map(|user| {
                    let keychain = login_keychain(Path::new(&user.home));
                    let as_user = || {
                        let mut cmd = Command::new("security");
                        if unsafe { libc::geteuid() } == 0 {
                            user.apply_to_command(&mut cmd);
                        }
                        cmd
                    };
                    // `-t` also drops the trust setting, but may need a
                    // password the user can refuse; the certificate goes either way.
                    match delete_all(as_user(), &keychain, true) {
                        0 => delete_all(as_user(), &keychain, false),
                        n => n,
                    }
                })
                .sum::<usize>()
    }

    fn is_trusted(&self, ca_cert_path: &Path) -> Option<bool> {
        Some(is_trusted(ca_cert_path))
    }

    fn store_name(&self) -> &'static str {
        "your login keychain"
    }

    fn install_notice(&self) -> &'static str {
        "fghj's root CA isn't trusted yet — macOS will ask for your password to trust it"
    }

    fn untrusted_hint(&self) -> &'static str {
        "run `fghj doctor --fix` from a terminal — it asks macOS to trust the CA, \
         which shows a password dialog"
    }
}

fn login_keychain(home: &Path) -> PathBuf {
    home.join("Library/Keychains/login.keychain-db")
}

/// Loops: `delete-certificate` removes one match per call, and repeated installs
/// leave several CAs. A non-zero exit means none are left.
fn delete_all(mut security: Command, keychain: &Path, with_trust: bool) -> usize {
    security.arg("delete-certificate");
    if with_trust {
        security.arg("-t");
    }
    security
        .args(["-c", COMMON_NAME])
        .arg(keychain)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut removed = 0;
    while removed < 32 && security.status().map(|s| s.success()).unwrap_or(false) {
        removed += 1;
    }
    removed
}

/// `verify-cert` evaluates trust without prompting, so this is safe to call
/// anytime. As the calling user: their trust settings, and any machine-wide
/// ones an earlier fghj made.
fn is_trusted(ca_cert_path: &Path) -> bool {
    Command::new("security")
        .args(["verify-cert", "-c"])
        .arg(ca_cert_path)
        // `output`, not `status`, to keep its chatter out of the log.
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Checks first, since `add-trusted-cert` always prompts.
fn install_trust(ca_cert_path: &Path) -> Result<()> {
    if is_trusted(ca_cert_path) {
        return Ok(());
    }
    if unsafe { libc::geteuid() } == 0 {
        anyhow::bail!(
            "not trusting fghj's CA as root: it goes in your own login keychain. \
             Run `fghj doctor --fix` without sudo"
        );
    }
    let home = std::env::var_os("HOME").context("HOME isn't set")?;
    let keychain = login_keychain(Path::new(&home));

    let status = Command::new("security")
        .args(["add-trusted-cert", "-r", "trustRoot", "-k"])
        .arg(&keychain)
        .arg(ca_cert_path)
        .status()
        .context("failed to run `security add-trusted-cert`")?;
    if !status.success() {
        anyhow::bail!(
            "`security add-trusted-cert` failed for {cert}\n\
             macOS asks for your password before trusting a root, so it needs a terminal \
             in your login session (not ssh). Run `fghj doctor --fix`, or:\n\
             \n\
             \x20   security add-trusted-cert -r trustRoot -k {keychain} {cert}",
            cert = ca_cert_path.display(),
            keychain = keychain.display(),
        );
    }
    println!("trusted the fghj local CA in your login keychain");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_is_the_users_login_keychain() {
        assert_eq!(
            login_keychain(Path::new("/Users/someone")),
            Path::new("/Users/someone/Library/Keychains/login.keychain-db")
        );
    }
}
