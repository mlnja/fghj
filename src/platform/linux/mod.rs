//! Linux, starting with Ubuntu LTS: installed through apt and run by systemd.
//!
//! DNS routing goes through systemd-resolved. The CA goes into the system
//! store where it's Debian's `update-ca-certificates`, and into the
//! browsers' own NSS databases. Loopback needs nothing: all of `127/8`
//! already routes to `lo`.

use super::{Platform, TrustStore, unsupported};

mod apt;
mod ca_certificates;
mod nss;
mod resolved;
mod runtime_dir;

pub(super) fn platform() -> Platform {
    Platform {
        dns: Box::new(resolved::ResolvedLink),
        trust: system_trust(),
        browser_trust: Box::new(nss::NssDatabases),
        loopback: Box::new(unsupported::NoLoopback),
        session: Box::new(runtime_dir::RuntimeDir::system()),
        packaging: Box::new(apt::Apt),
    }
}

/// The first store this machine has the tool for. Another distro's is a
/// file of its own and a line here.
fn system_trust() -> Box<dyn TrustStore> {
    if ca_certificates::CaCertificates::detect() {
        Box::new(ca_certificates::CaCertificates)
    } else {
        Box::new(unsupported::NoTrustStore)
    }
}
