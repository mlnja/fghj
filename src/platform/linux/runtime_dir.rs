//! Linux paths, and the per-user runtime directory (`/run/user/<uid>`) where
//! login-session ssh-agents keep their sockets.

use std::path::{Path, PathBuf};

use crate::platform::Session;

/// Under `/run/user/<uid>`, in the order they're tried.
const SESSION_AGENTS: &[&str] = &[
    // GNOME's, Ubuntu 24.04 and later.
    "gcr/ssh",
    // GNOME Keyring's, Ubuntu 22.04.
    "keyring/ssh",
    // OpenSSH's own `ssh-agent.service` user unit.
    "openssh_agent",
    // gpg-agent with ssh support.
    "gnupg/S.gpg-agent.ssh",
];

pub(super) struct RuntimeDir {
    run_user: PathBuf,
}

impl RuntimeDir {
    pub(super) fn system() -> Self {
        Self {
            run_user: PathBuf::from("/run/user"),
        }
    }
}

impl Session for RuntimeDir {
    fn state_root(&self) -> PathBuf {
        PathBuf::from("/var/lib/fghjd")
    }

    /// `/dev/console` is root's on Linux, so there's nobody to read off it.
    fn console_uid(&self) -> Option<u32> {
        None
    }

    fn session_ssh_agent(&self, uid: u32, is_live: &dyn Fn(&Path) -> bool) -> Option<String> {
        let dir = self.run_user.join(uid.to_string());
        SESSION_AGENTS
            .iter()
            .map(|agent| dir.join(agent))
            .find(|candidate| is_live(candidate))
            .map(|found| found.to_string_lossy().into_owned())
    }

    fn is_session_agent(&self, sock: &str) -> bool {
        let Ok(rest) = Path::new(sock).strip_prefix(&self.run_user) else {
            return false;
        };
        let mut parts = rest.iter();
        let is_uid = parts
            .next()
            .and_then(|uid| uid.to_str())
            .is_some_and(|uid| uid.parse::<u32>().is_ok());
        is_uid
            && SESSION_AGENTS
                .iter()
                .any(|agent| parts.as_path() == Path::new(agent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_live_agent_in_the_runtime_dir_wins() {
        let live = |p: &Path| {
            p == Path::new("/run/user/1000/openssh_agent")
                || p == Path::new("/run/user/1000/gnupg/S.gpg-agent.ssh")
        };
        let dir = RuntimeDir::system();
        assert_eq!(
            dir.session_ssh_agent(1000, &live).as_deref(),
            Some("/run/user/1000/openssh_agent")
        );
        assert_eq!(dir.session_ssh_agent(1001, &live), None);
    }

    #[test]
    fn only_the_known_agent_sockets_count_as_the_session_agent() {
        let dir = RuntimeDir::system();
        assert!(dir.is_session_agent("/run/user/1000/gcr/ssh"));
        assert!(dir.is_session_agent("/run/user/1000/gnupg/S.gpg-agent.ssh"));
        assert!(!dir.is_session_agent("/run/user/1000/gcr/ssh/extra"));
        assert!(!dir.is_session_agent("/run/user/me/gcr/ssh"));
        assert!(!dir.is_session_agent("/home/me/.1password/agent.sock"));
    }
}
