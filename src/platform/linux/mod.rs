//! Linux, starting with Ubuntu LTS: installed through apt and run by systemd.
//!
//! DNS routing goes through systemd-resolved. CA trust isn't implemented yet
//! and borrows the `unsupported` backend, which says so in `fghj doctor`.
//! Loopback needs nothing: all of `127/8` already routes to `lo`.

use super::{Platform, unsupported};

mod apt;
mod resolved;
mod runtime_dir;

pub(super) fn platform() -> Platform {
    Platform {
        dns: Box::new(resolved::ResolvedLink),
        trust: Box::new(unsupported::NoTrustStore),
        loopback: Box::new(unsupported::NoLoopback),
        session: Box::new(runtime_dir::RuntimeDir::system()),
        packaging: Box::new(apt::Apt),
    }
}
