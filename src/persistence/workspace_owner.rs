use std::path::Path;

use serde::{Deserialize, Serialize};

/// The real user who ran `fghj wire`, captured at registration time by the
/// unprivileged `fghj` CLI (which has the correct uid/env) and persisted so
/// `fghjd` — running as root — can later drop privileges back to this user
/// before shelling out to `git clone` against a remote the daemon itself has
/// no credentials for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceOwner {
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub ssh_auth_sock: Option<String>,
}

impl WorkspaceOwner {
    /// Configures `cmd` to run as this user instead of whoever spawned it
    /// (`fghjd`, running as root) — root bypasses the file permission check
    /// on this user's ssh-agent socket, so it can use their credentials even
    /// though it isn't them, provided it knows where to look (`HOME`,
    /// `SSH_AUTH_SOCK`).
    pub fn apply_to_command(&self, cmd: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        cmd.uid(self.uid).gid(self.gid).env("HOME", &self.home);
        match live_ssh_auth_sock(self.uid, self.ssh_auth_sock.as_deref()) {
            Some(sock) => {
                cmd.env("SSH_AUTH_SOCK", sock);
            }
            None => {
                cmd.env_remove("SSH_AUTH_SOCK");
            }
        }
    }
}

/// Finds a live ssh-agent socket for `uid`, rather than trusting `hint` (the
/// `SSH_AUTH_SOCK` captured from one shell's environment at `fghj wire`
/// time) blindly forever. `hint` can go stale — the agent that created it may
/// have been restarted — and re-deriving it fresh is what makes this actually
/// track the user rather than a snapshot of one of their terminal sessions.
///
/// macOS's system agent is bound to the *login* session (via `launchd`), not
/// any one terminal, and lives at a deterministic, discoverable path, so a
/// dead `hint` can be recovered by scanning for it; on other platforms there
/// is no equivalent well-known path, so a dead hint is simply unusable.
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

/// Disables all interactive ssh prompting (`BatchMode`) and auto-accepts
/// unknown host keys, so a `git clone` subprocess either succeeds or fails
/// fast and visibly instead of hanging forever on a host-key prompt that's
/// written straight to the parent process's controlling tty — invisible to
/// (and unanswerable from) anything capturing its piped stdout/stderr.
/// Worth applying even when also running as the real owner via
/// [`WorkspaceOwner::apply_to_command`], as a second line of defense.
pub fn harden_git_ssh(cmd: &mut std::process::Command) {
    cmd.env(
        "GIT_SSH_COMMAND",
        "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
    );
}
