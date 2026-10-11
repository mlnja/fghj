//! Thin wrappers over bollard, one file per kind of Docker object. Everything
//! is re-exported here so callers keep writing `docker::run_container` etc.

mod container;
mod image;
mod network;
mod volume;

pub use container::*;
pub use image::*;
pub use network::*;
pub use volume::*;

/// A `Docker` handle for tests that never actually talk to Docker — the ones
/// about bookkeeping that happens to live behind a type owning a client.
///
/// `connect_with_local_defaults` cannot serve this purpose: bollard resolves
/// the unix socket at *construction* and returns `SocketNotFoundError` when it
/// isn't there, so a test that sends no request still fails on any machine
/// without Docker — which is every CI runner this project can use, for the
/// reasons `concepts/release-and-delivery.md` sets out. The http transport
/// does no such check. Port 1 is deliberate: nothing listens there, so a
/// request that shouldn't happen fails loudly instead of quietly reaching a
/// real daemon.
#[cfg(test)]
pub(crate) fn undialled_client() -> bollard::Docker {
    bollard::Docker::connect_with_http("http://127.0.0.1:1", 1, bollard::API_DEFAULT_VERSION)
        .expect("constructing an http transport cannot fail")
}

/// Cheap, dependency-free uniqueness for throwaway container names and image
/// tags in the Docker-backed tests — they never run concurrently with each
/// other in practice, but a stale leftover from a previous crashed run
/// shouldn't collide either.
#[cfg(test)]
pub(crate) fn rand_suffix() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0)
}
