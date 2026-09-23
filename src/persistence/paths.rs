use std::path::PathBuf;

/// The durable root `fghjd` owns on this machine — `/var/lib/fghjd` in
/// spirit, but resolved to its real, symlink-free path. On macOS, `/var` is
/// itself a symlink to `/private/var`; bind-mounting a *file* through that
/// symlink (e.g. a workspace's own `.fghj.yaml` mounting
/// `/var/lib/fghjd/ca/bundle.pem`, the one path this daemon documents as
/// stable enough to reference by name) makes OrbStack's mount-type
/// detection misfire with a spurious "not a directory" error, even though
/// both sides of the mount are genuinely regular files. Resolving the
/// symlink ourselves, once, here, means every path built from this root —
/// including the one users write literally into their own `volumes:` — is
/// already immune to it, on every container engine, not just the ones that
/// happen not to trip over the symlink. Linux has no such `/var` symlink, so
/// this is a no-op there.
pub fn fghjd_root() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/private/var/lib/fghjd")
    } else {
        PathBuf::from("/var/lib/fghjd")
    }
}

/// Not `/var/run`: that's commonly a tmpfs wiped on reboot, which would
/// defeat the point of tracking workspaces across a restart. Taken as a
/// parameter (rather than hardcoded) in `load_index`/`save_index` so tests
/// can point it at a tempdir instead of the real root-owned path.
pub fn default_index_path() -> PathBuf {
    fghjd_root().join("workspaces.json")
}

/// Alongside the CA and the workspace index, not `/var/run`, for the same
/// reason as `default_index_path`: this needs to survive a reboot.
pub fn default_state_path() -> PathBuf {
    fghjd_root().join("daemon-state.json")
}
