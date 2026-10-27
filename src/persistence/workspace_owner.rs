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
    /// Whoever owns `folder`. Stands in when no owner was recorded: the
    /// workspace predates recording one, or its `.fghj` was recreated since,
    /// and only `fghj wire` records one. `None` for a root-owned folder,
    /// which leaves nobody to act as.
    pub fn of_folder(folder: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(folder).ok()?;
        if meta.uid() == 0 {
            return None;
        }
        Some(Self {
            uid: meta.uid(),
            gid: meta.gid(),
            home: home_dir_of(meta.uid())?,
            // Found when used; see `live_ssh_auth_sock`.
            ssh_auth_sock: None,
        })
    }

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

/// The recorded owner, else whoever owns the workspace folder.
pub async fn workspace_owner(
    db: std::sync::Arc<crate::persistence::WorkspaceDb>,
    folder: &Path,
) -> Option<WorkspaceOwner> {
    match db.load_owner().await {
        Ok(Some(owner)) => Some(owner),
        _ => WorkspaceOwner::of_folder(folder),
    }
}

pub(crate) fn home_dir_of(uid: u32) -> Option<String> {
    // SAFETY: the static buffer is copied out before any other libc call.
    unsafe {
        let pw = libc::getpwuid(uid as libc::uid_t);
        if pw.is_null() || (*pw).pw_dir.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*pw).pw_dir)
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

/// `hint` goes stale when the agent restarts, and `of_folder` has none. Where
/// the login session's agent lives at a discoverable path, it's found anyway.
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

    crate::platform::current()
        .session
        .session_ssh_agent(uid, &is_live_socket_for_uid)
}

/// `BatchMode` and auto-accepted host keys, so a clone fails fast instead of
/// hanging on a prompt nobody can see.
pub fn harden_git_ssh(cmd: &mut std::process::Command) {
    cmd.env(
        "GIT_SSH_COMMAND",
        "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_stands_in_for_its_owner() {
        let uid = unsafe { libc::getuid() };
        let tmp = tempfile::tempdir().unwrap();
        let owner = WorkspaceOwner::of_folder(tmp.path());
        if uid == 0 {
            assert!(owner.is_none());
            return;
        }
        let owner = owner.unwrap();
        assert_eq!(owner.uid, uid);
        assert_eq!(Some(owner.home), home_dir_of(uid));
        assert!(WorkspaceOwner::of_folder(Path::new("/")).is_none());
    }
}
