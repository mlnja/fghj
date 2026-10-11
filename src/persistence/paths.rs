use std::path::PathBuf;

/// `/var/lib/fghjd` with symlinks resolved: on macOS `/var` is a symlink, and
/// bind-mounting a file through it trips OrbStack ("not a directory").
pub fn fghjd_root() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/private/var/lib/fghjd")
    } else {
        PathBuf::from("/var/lib/fghjd")
    }
}

/// Not `/var/run`, which is often wiped on reboot.
pub fn default_index_path() -> PathBuf {
    fghjd_root().join("workspaces.json")
}

pub fn default_state_path() -> PathBuf {
    fghjd_root().join("daemon-state.json")
}
