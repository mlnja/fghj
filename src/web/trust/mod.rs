//! Installing fghj's root CA into the OS trust store, and removing it on
//! `fghj uninstall`. Only macOS is implemented.

use std::path::Path;

use anyhow::Result;

mod macos;

/// A no-op when already trusted.
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

pub fn remove() -> usize {
    if cfg!(target_os = "macos") {
        macos::remove_trust()
    } else {
        0
    }
}

/// `None` where unsupported, so `doctor` can tell it from "not trusted".
pub fn is_trusted(ca_cert_path: &Path) -> Option<bool> {
    if cfg!(target_os = "macos") {
        Some(macos::is_trusted(ca_cert_path))
    } else {
        None
    }
}

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

    #[test]
    fn a_freshly_minted_ca_is_not_trusted() {
        let tmp = tempfile::tempdir().unwrap();
        crate::web::ca::ensure_ca(tmp.path()).unwrap();
        let cert = crate::web::ca::ca_cert_path(tmp.path());

        let expected = cfg!(target_os = "macos").then_some(false);
        assert_eq!(is_trusted(&cert), expected);
    }

    /// Not run on macOS, where `remove` would touch the real keychain.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_platforms_install_and_remove_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        install(&tmp.path().join("ca.pem")).unwrap();
        assert_eq!(remove(), 0);
    }
}
