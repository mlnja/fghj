//! The `fghjd` control daemon.
//!
//! See `concepts/control-api-and-cli.md`. This module is the daemon's own
//! state and lifecycle — which workspaces exist, whether the daemon is
//! actively occupying ports/DNS, and the reconcilers that keep reality in
//! step. The HTTP surface it serves lives in `web`; [`bootstrap`] is where
//! the two meet.

use std::path::PathBuf;

use crate::persistence;

pub mod bootstrap;
pub mod control;
pub mod reconcile;
pub mod registry;
pub mod routing;
pub mod workspace_id;

pub use bootstrap::{connect_docker, run_control_api};
pub use control::DaemonControl;
pub use registry::WorkspaceRegistry;
pub use workspace_id::workspace_id;

/// Where `fghjd`'s control API listens — a Unix socket rather than a TCP
/// port, dockerd-style: it's local-machine-only by nature (no port to pick,
/// collide with, or scan) and access control is a filesystem permission
/// (see `run_control_api`'s `chmod` after bind) instead of "trust anything
/// that can reach 127.0.0.1". Only meaningful while `fghjd` is alive, so
/// `/var/run` (not the durable `/var/lib/fghjd` the CA lives under) is the
/// right place.
pub fn socket_path() -> PathBuf {
    PathBuf::from("/var/run/fghjd.sock")
}

/// Durable storage for the local CA — must survive a reboot, or every
/// `fghjd` restart would need the user to re-approve a brand new CA in
/// Keychain Access. `pub(crate)` so `runs.rs` can bind-mount it (read-only)
/// into a run's sidecar proxy container.
pub(crate) fn ca_dir() -> PathBuf {
    persistence::fghjd_root().join("ca")
}
