//! Installing fghj's root CA ([`crate::web::ca`]) into the OS trust store, so
//! browsers accept the certificates it issues without a warning — and
//! taking it out again on `fghj uninstall`.
//!
//! Kept apart from `ca` because everything here is a privileged,
//! platform-specific side effect, while `ca` itself is plain, testable file
//! and crypto work. Same shape as `dns`'s resolver config and `raw_net`'s
//! backend: callers use the functions below and never see which mechanism
//! is underneath. Only macOS is implemented; elsewhere `install` says so and
//! the rest report nothing installed.

use std::path::Path;

use anyhow::Result;

mod macos;

/// Ensures `ca_cert_path` is trusted as a root. Safe to call on every
/// `fghjd` start — a no-op when trust is already in place.
pub fn install(ca_cert_path: &Path) -> Result<()> {
    if cfg!(target_os = "macos") {
        macos::install_trust(ca_cert_path)
    } else {
        eprintln!(
            "fghjd: automatic system trust installation isn't implemented on this platform yet — \
             trust {} manually so browsers accept fghj's issued certificates",
            ca_cert_path.display()
        );
        Ok(())
    }
}

/// The inverse of [`install`]: removes every trusted certificate named
/// [`crate::web::ca::COMMON_NAME`], returning how many it removed.
pub fn remove() -> usize {
    if cfg!(target_os = "macos") {
        macos::remove_trust()
    } else {
        0
    }
}

/// Whether `ca_cert_path` is currently trusted as a root — a read-only
/// check that never prompts. `None` on a platform where [`install`] does
/// nothing, so `doctor` can tell "not trusted" from "not supported here".
pub fn is_trusted(ca_cert_path: &Path) -> Option<bool> {
    if cfg!(target_os = "macos") {
        Some(macos::is_trusted(ca_cert_path))
    } else {
        None
    }
}

/// The trust store [`install`] writes to, as a user would recognise it —
/// for `uninstall`'s confirmation prompt and `doctor`'s report.
pub fn store_name() -> &'static str {
    if cfg!(target_os = "macos") {
        macos::SYSTEM_KEYCHAIN
    } else {
        "the system trust store"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_name_names_the_platform_store() {
        if cfg!(target_os = "macos") {
            assert_eq!(store_name(), "/Library/Keychains/System.keychain");
        } else {
            assert_eq!(store_name(), "the system trust store");
        }
    }

    /// A CA minted a moment ago can't be trusted yet — on macOS this runs
    /// the real `security verify-cert`, which only evaluates trust and never
    /// prompts or modifies the keychain.
    #[test]
    fn a_freshly_minted_ca_is_not_trusted() {
        let tmp = tempfile::tempdir().unwrap();
        crate::web::ca::ensure_ca(tmp.path()).unwrap();
        let cert = crate::web::ca::ca_cert_path(tmp.path());

        let expected = cfg!(target_os = "macos").then_some(false);
        assert_eq!(is_trusted(&cert), expected);
    }

    /// Off macOS every entry point is a no-op. Not run on macOS, where
    /// `remove` would delete real certificates from the System keychain.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_platforms_install_and_remove_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        install(&tmp.path().join("ca.pem")).unwrap();
        assert_eq!(remove(), 0);
    }
}
