use std::path::{Path, PathBuf};

/// Where things live, and who the logged-in user is, as root `fghjd` sees it.
pub trait Session: Send + Sync {
    /// `/var/lib/fghjd` with symlinks resolved: bind-mounting a file through a
    /// symlink trips some Docker engines.
    fn state_root(&self) -> PathBuf;

    /// The uid at the console, whose docker context fghjd follows. `None`
    /// when it can't tell, or it's root.
    fn console_uid(&self) -> Option<u32>;

    /// The login session's own ssh-agent for `uid`, found without being told:
    /// a recorded `SSH_AUTH_SOCK` goes stale when the agent restarts.
    /// `is_live` checks a candidate socket belongs to `uid`.
    fn session_ssh_agent(&self, uid: u32, is_live: &dyn Fn(&Path) -> bool) -> Option<String>;

    /// Whether `sock` is that session agent, rather than one `fghj wire` sent.
    fn is_session_agent(&self, sock: &str) -> bool;

    /// Docker sockets of `uid`'s own to try, as `unix://…`, when neither their
    /// docker context nor `/var/run/docker.sock` names a live one.
    fn docker_sockets(&self, uid: u32) -> Vec<String>;
}
