use std::path::Path;

use serde::{Deserialize, Serialize};

/// Captured by the unprivileged CLI so root `fghjd` can act as this user, e.g.
/// for `git clone` with their credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceOwner {
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub ssh_auth_sock: Option<String>,
}

impl WorkspaceOwner {
    /// Root bypasses the permission check on the user's ssh-agent socket; it just
    /// needs `HOME` and `SSH_AUTH_SOCK`.
    pub fn apply_to_command(&self, cmd: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        cmd.uid(self.uid).gid(self.gid).env("HOME", &self.home);
        match self.live_ssh_auth_sock() {
            Some(sock) => {
                cmd.env("SSH_AUTH_SOCK", sock);
            }
            None => {
                cmd.env_remove("SSH_AUTH_SOCK");
            }
        }
    }

    /// Public because bollard reads `SSH_AUTH_SOCK` from `fghjd`'s own environment.
    pub fn live_ssh_auth_sock(&self) -> Option<String> {
        live_ssh_auth_sock(self.uid, self.ssh_auth_sock.as_deref())
    }
}

/// `hint` goes stale when the agent restarts. On macOS the login-session agent
/// lives at a discoverable path, so a dead hint can be recovered; elsewhere it
/// can't.
fn live_ssh_auth_sock(uid: u32, hint: Option<&str>) -> Option<String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let is_live_socket_for_uid = |path: &Path| -> bool {
        std::fs::metadata(path)
            .map(|m| m.uid() == uid && m.file_type().is_socket())
            .unwrap_or(false)
    };

    if let Some(hint) = hint
        && is_live_socket_for_uid(Path::new(hint))
    {
        return Some(hint.to_string());
    }

    if cfg!(target_os = "macos") {
        let entries = std::fs::read_dir("/private/tmp").ok()?;
        for entry in entries.flatten() {
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with("com.apple.launchd.")
            {
                continue;
            }
            let candidate = entry.path().join("Listeners");
            if is_live_socket_for_uid(&candidate) {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }

    None
}

/// `BatchMode` and auto-accepted host keys, so a clone fails fast instead of
/// hanging on a prompt nobody can see.
pub fn harden_git_ssh(cmd: &mut std::process::Command) {
    cmd.env(
        "GIT_SSH_COMMAND",
        "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
    );
}
