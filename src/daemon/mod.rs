//! The `fghjd` control daemon. See `concepts/control-api-and-cli.md`.

use std::path::PathBuf;

use crate::persistence;

pub mod bootstrap;
mod capture;
pub mod control;
mod heal;
pub mod reconcile;
pub mod registry;
pub mod routing;
pub mod workspace_id;

pub use bootstrap::{connect_docker, run_control_api};
pub use control::DaemonControl;
pub use registry::WorkspaceRegistry;
pub use workspace_id::workspace_id;

/// Access control is a filesystem permission, dockerd-style. Under `/var/run`:
/// only meaningful while `fghjd` is alive.
pub fn socket_path() -> PathBuf {
    PathBuf::from("/var/run/fghjd.sock")
}

/// Durable, or every restart would need a fresh CA approved in Keychain Access.
pub fn ca_dir() -> PathBuf {
    persistence::fghjd_root().join("ca")
}

/// Not `ca_dir()`: this is mounted into every container, and `:ro` doesn't
/// stop a container reading `ca-key.pem`. The key must never have been here.
pub(crate) fn certs_dir() -> PathBuf {
    persistence::fghjd_root().join("certs")
}

/// Mounting is all fghj does: trusting an extra CA means replacing a file or
/// variable the image owns, which isn't fghj's call.
pub(crate) const CERTS_MOUNT: &str = "/etc/fghj/certs";
