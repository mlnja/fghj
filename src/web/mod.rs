//! Everything fghj serves over HTTP: the control API, the embedded UI, the TLS
//! proxy, and the local CA behind it.

pub mod api;
pub mod ca;
pub mod mime;
pub mod proxy;
pub mod query;
pub mod trust;
pub mod ui;
