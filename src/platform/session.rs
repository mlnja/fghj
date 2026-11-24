use std::ffi::OsString;
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

    /// The command line that runs `argv` as `uid`/`gid`, from root, inside that
    /// user's login session (where their credential store answers) with only
    /// `env`. `None` where fghj doesn't know how.
    fn as_user_in_session(
        &self,
        uid: u32,
        gid: u32,
        env: &[(String, OsString)],
        argv: &[OsString],
    ) -> Option<Vec<OsString>>;
}

/// `KEY=value` arguments for `env -i`.
pub(crate) fn env_pairs(env: &[(String, OsString)]) -> impl Iterator<Item = OsString> + '_ {
    env.iter().map(|(key, value)| {
        let mut pair = OsString::from(format!("{key}="));
        pair.push(value);
        pair
    })
}
