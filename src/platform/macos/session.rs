//! macOS paths and login sessions.

use std::path::{Path, PathBuf};

use crate::platform::Session;

pub(super) struct MacSession;

impl Session for MacSession {
    /// `/var` is a symlink, and bind-mounting a file through it trips OrbStack
    /// ("not a directory").
    fn state_root(&self) -> PathBuf {
        PathBuf::from("/private/var/lib/fghjd")
    }

    /// Whoever owns `/dev/console` is logged in at the screen.
    fn console_uid(&self) -> Option<u32> {
        use std::os::unix::fs::MetadataExt;
        let uid = std::fs::metadata("/dev/console").ok()?.uid();
        (uid != 0).then_some(uid)
    }

    /// launchd gives every login session an agent at a discoverable path.
    fn session_ssh_agent(&self, _uid: u32, is_live: &dyn Fn(&Path) -> bool) -> Option<String> {
        // `/private/tmp` up to macOS 15, `/private/var/run` since.
        for dir in ["/private/var/run", "/private/tmp"] {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(LAUNCHD_SOCKET_PREFIX)
                {
                    continue;
                }
                let candidate = entry.path().join("Listeners");
                if is_live(&candidate) {
                    return Some(candidate.to_string_lossy().into_owned());
                }
            }
        }
        None
    }

    fn is_session_agent(&self, sock: &str) -> bool {
        sock.contains(&format!("/{LAUNCHD_SOCKET_PREFIX}"))
    }
}

const LAUNCHD_SOCKET_PREFIX: &str = "com.apple.launchd.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launchd_agent_is_the_session_agent() {
        assert!(MacSession.is_session_agent("/private/tmp/com.apple.launchd.AbC123/Listeners"));
        assert!(!MacSession.is_session_agent("/Users/me/.1password/agent.sock"));
    }
}
