//! Linux, starting with Ubuntu LTS: installed through apt and run by systemd.
//!
//! DNS routing, CA trust and raw-port NAT aren't implemented yet and borrow
//! the `unsupported` backends, which say so in `fghj doctor`. Loopback
//! needs nothing: all of `127/8` already routes to `lo`.

use super::{Platform, unsupported};

mod apt;
mod runtime_dir;

pub(super) fn platform() -> Platform {
    Platform {
        dns: Box::new(unsupported::NoDnsRouting),
        trust: Box::new(unsupported::NoTrustStore),
        nat: Box::new(unsupported::NoRawNat::default()),
        loopback: Box::new(unsupported::NoLoopback),
        session: Box::new(runtime_dir::RuntimeDir::system()),
        packaging: Box::new(apt::Apt),
    }
}
