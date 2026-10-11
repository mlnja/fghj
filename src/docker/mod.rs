//! Thin wrappers over bollard, one file per kind of Docker object.

mod container;
mod image;
mod network;
mod volume;

pub use container::*;
pub use image::*;
pub use network::*;
pub use volume::*;

/// Never dials Docker. `connect_with_local_defaults` fails without a socket;
/// port 1 makes an accidental request fail loudly.
#[cfg(test)]
pub(crate) fn undialled_client() -> bollard::Docker {
    bollard::Docker::connect_with_http("http://127.0.0.1:1", 1, bollard::API_DEFAULT_VERSION)
        .expect("constructing an http transport cannot fail")
}

#[cfg(test)]
pub(crate) fn rand_suffix() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0)
}
