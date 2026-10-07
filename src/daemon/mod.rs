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
/// Keychain Access. `pub(crate)` so `runs/` can bind-mount it (read-only)
/// into a run's sidecar proxy container.
pub(crate) fn ca_dir() -> PathBuf {
    persistence::fghjd_root().join("ca")
}

/// Where the container-facing copies of the CA's two key-free trust files
/// live — `cert.pem` and `bundle.pem`, exactly as `ca::refresh_trust_files`
/// writes them, and nothing else.
///
/// That "nothing else" is the entire reason this is not just `ca_dir()`,
/// which is where the same two files are also written: `ca_dir()` holds
/// `ca-key.pem`, and `fghj` bind-mounts this directory into *every*
/// container it starts. `:ro` stops a container writing to a mount, not
/// reading from it, and a container running as root reads a root-owned
/// `0600` file through a bind mount quite happily. So the directory that
/// gets mounted has to be one the private key was never in.
pub(crate) fn certs_dir() -> PathBuf {
    persistence::fghjd_root().join("certs")
}

/// Where `certs_dir()` appears inside every container — an fghj-namespaced
/// path, so it collides with nothing an image already ships, and a stable
/// one, so a `.fghj.yaml` can name a file under it (in `environment:`) and
/// keep working.
///
/// Mounting is as far as this goes: nothing fghj does activates these certs.
/// Trusting an extra CA on Unix means *replacing* a file or a variable the
/// image owns (`SSL_CERT_FILE`, `/etc/ssl/certs/ca-certificates.crt`) —
/// never adding to it, Node's `NODE_EXTRA_CA_CERTS` being the lone
/// exception — and silently swapping a container's trust store for fghj's
/// idea of one is not a thing to do behind an author's back. The files are
/// simply *there*, costing nothing, for the configs that ask.
pub(crate) const CERTS_MOUNT: &str = "/etc/fghj/certs";
