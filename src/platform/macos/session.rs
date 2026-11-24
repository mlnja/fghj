//! macOS paths and login sessions.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::platform::Session;

pub(super) struct MacSession;

impl Session for MacSession {
    /// `/var` is a symlink, and bind-mounting a file through it trips OrbStack
    /// ("not a directory").
    fn state_root(&self) -> PathBuf {
        PathBuf::from("/private/var/lib/fghjd")
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

    /// `launchctl asuser` puts the command in the user's bootstrap namespace,
    /// where their keychain answers; it needs root, so `sudo` drops to the
    /// user after it. `env -i` leaves root's environment behind.
    fn as_user_in_session(
        &self,
        uid: u32,
        _gid: u32,
        env: &[(String, OsString)],
        argv: &[OsString],
    ) -> Option<Vec<OsString>> {
        let mut line: Vec<OsString> = [
            "/bin/launchctl",
            "asuser",
            &uid.to_string(),
            "/usr/bin/sudo",
            "-u",
            &format!("#{uid}"),
            "-H",
            "/usr/bin/env",
            "-i",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        line.extend(crate::platform::session::env_pairs(env));
        line.extend(argv.iter().cloned());
        Some(line)
    }
}

const LAUNCHD_SOCKET_PREFIX: &str = "com.apple.launchd.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_runs_as_the_user_in_their_launchd_session() {
        let env = [(String::from("HOME"), OsString::from("/Users/me"))];
        let argv = [
            OsString::from("/usr/local/bin/docker"),
            OsString::from("version"),
        ];
        let line = MacSession.as_user_in_session(501, 20, &env, &argv).unwrap();
        assert_eq!(
            line,
            [
                "/bin/launchctl",
                "asuser",
                "501",
                "/usr/bin/sudo",
                "-u",
                "#501",
                "-H",
                "/usr/bin/env",
                "-i",
                "HOME=/Users/me",
                "/usr/local/bin/docker",
                "version",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn the_launchd_agent_is_the_session_agent() {
        assert!(MacSession.is_session_agent("/private/tmp/com.apple.launchd.AbC123/Listeners"));
        assert!(!MacSession.is_session_agent("/Users/me/.1password/agent.sock"));
    }
}
