use std::path::{Path, PathBuf};

/// Where things live, and where a user's login session keeps its sockets.
pub trait Session: Send + Sync {
    /// `/var/lib/fghjd` with symlinks resolved: bind-mounting a file through a
    /// symlink trips some Docker engines.
    fn state_root(&self) -> PathBuf;

    /// The login session's own ssh-agent for `uid`, found without being told:
    /// a recorded `SSH_AUTH_SOCK` goes stale when the agent restarts.
    /// `is_live` checks a candidate socket belongs to `uid`.
    fn session_ssh_agent(&self, uid: u32, is_live: &dyn Fn(&Path) -> bool) -> Option<String>;

    /// Whether `sock` is that session agent, rather than one `fghj wire` sent.
    fn is_session_agent(&self, sock: &str) -> bool;
}
