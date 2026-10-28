//! macOS: `/etc/resolver`, the System keychain and `lo0` aliases,
//! installed through Homebrew and run by launchd.

use super::{Platform, run};

mod homebrew;
mod keychain;
mod lo0;
mod pf;
mod resolver_dir;
mod session;

pub(super) fn platform() -> Platform {
    Platform {
        dns: Box::new(resolver_dir::ResolverDir),
        trust: Box::new(keychain::SystemKeychain),
        legacy_nat: Box::new(pf::RetiredPf),
        loopback: Box::new(lo0::Lo0),
        session: Box::new(session::MacSession),
        packaging: Box::new(homebrew::Homebrew),
    }
}
