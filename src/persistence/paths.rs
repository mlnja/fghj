use std::path::PathBuf;

/// `/var/lib/fghjd` with symlinks resolved; see [`crate::platform::Session::state_root`].
pub fn fghjd_root() -> PathBuf {
    crate::platform::current().session.state_root()
}

/// Not `/var/run`, which is often wiped on reboot.
pub fn default_index_path() -> PathBuf {
    fghjd_root().join("workspaces.json")
}

pub fn default_state_path() -> PathBuf {
    fghjd_root().join("daemon-state.json")
}
