//! Everything fghj does differently per OS, one trait per capability.
//!
//! The rest of the crate goes through [`current`] and never checks the OS
//! itself. Each platform is a directory of implementations, and [`select`] is
//! the only place that picks one: supporting a new OS means a new directory
//! and one match arm, not edits to shared code.
//!
//! Every implementation compiles on every target. They only shell out and
//! parse what comes back, so their tests run anywhere.

use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};

mod dns;
mod linux;
mod loopback;
mod macos;
mod packaging;
mod session;
mod trust;
mod unsupported;

pub use dns::{DnsRouting, OsRouting};
pub use loopback::Loopback;
pub use packaging::Packaging;
pub use session::Session;
pub use trust::TrustStore;

/// One implementation of each capability. Constructing one must not touch the
/// system: the CLI builds it too, unprivileged.
pub struct Platform {
    pub dns: Box<dyn DnsRouting>,
    pub trust: Box<dyn TrustStore>,
    pub loopback: Box<dyn Loopback>,
    pub session: Box<dyn Session>,
    pub packaging: Box<dyn Packaging>,
}

pub fn current() -> &'static Platform {
    static PLATFORM: OnceLock<Platform> = OnceLock::new();
    PLATFORM.get_or_init(select)
}

fn select() -> Platform {
    match std::env::consts::OS {
        "macos" => macos::platform(),
        "linux" => linux::platform(),
        _ => unsupported::platform(),
    }
}

fn run(cmd: &mut Command) -> Result<()> {
    let output = cmd
        .output()
        .with_context(|| format!("failed to run {cmd:?}"))?;
    if !output.status.success() {
        bail!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Against the real OS, so they hold for whichever platform runs them.
#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn a_freshly_minted_ca_is_not_trusted() {
        let tmp = tempfile::tempdir().unwrap();
        crate::web::ca::ensure_ca(tmp.path()).unwrap();
        let cert = crate::web::ca::ca_cert_path(tmp.path());
        assert_ne!(current().trust.is_trusted(&cert), Some(true));
    }

    /// `192.0.2.1` is TEST-NET-1, which nothing assigns to loopback.
    #[test]
    fn loopback_probe_finds_localhost_and_not_test_net() {
        let loopback = &current().loopback;
        if let Some(localhost) = loopback.alias_present(Ipv4Addr::LOCALHOST) {
            assert!(localhost.unwrap());
        }
        if let Some(test_net) = loopback.alias_present(Ipv4Addr::new(192, 0, 2, 1)) {
            assert!(!test_net.unwrap());
        }
    }
}
