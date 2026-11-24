//! Linux paths, and the per-user runtime directory (`/run/user/<uid>`) where
//! login-session ssh-agents keep their sockets.

use std::ffi::OsString;
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

    fn session_ssh_agent(&self, uid: u32, is_live: &dyn Fn(&Path) -> bool) -> Option<String> {
        let dir = self.run_user.join(uid.to_string());
        SESSION_AGENTS
            .iter()
            .map(|agent| dir.join(agent))
            .find(|candidate| is_live(candidate))
            .map(|found| found.to_string_lossy().into_owned())
    }

    /// `setpriv` is util-linux, so always there, unlike `sudo`. `--init-groups`
    /// keeps the user's supplementary groups, `docker` among them. A credential
    /// helper on the session bus (`secretservice`, `pass` via gpg-agent) needs
    /// the user's runtime dir and bus, when they're logged in.
    fn as_user_in_session(
        &self,
        uid: u32,
        gid: u32,
        env: &[(String, OsString)],
        argv: &[OsString],
    ) -> Option<Vec<OsString>> {
        let mut line: Vec<OsString> = [
            String::from("/usr/bin/setpriv"),
            format!("--reuid={uid}"),
            format!("--regid={gid}"),
            String::from("--init-groups"),
            String::from("/usr/bin/env"),
            String::from("-i"),
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        line.extend(crate::platform::session::env_pairs(env));
        let runtime = self.run_user.join(uid.to_string());
        if runtime.is_dir() {
            let mut dir = OsString::from("XDG_RUNTIME_DIR=");
            dir.push(&runtime);
            line.push(dir);
            let bus = runtime.join("bus");
            if bus.exists() {
                let mut address = OsString::from("DBUS_SESSION_BUS_ADDRESS=unix:path=");
                address.push(&bus);
                line.push(address);
            }
        }
        line.extend(argv.iter().cloned());
        Some(line)
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
    fn a_command_runs_as_the_user_with_their_groups_and_session_bus() {
        let run_user = tempfile::tempdir().unwrap();
        let runtime = run_user.path().join("1000");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::write(runtime.join("bus"), "").unwrap();
        let dir = RuntimeDir {
            run_user: run_user.path().to_path_buf(),
        };
        let env = [(String::from("HOME"), OsString::from("/home/me"))];
        let argv = [OsString::from("/usr/bin/docker"), OsString::from("version")];
        let line = dir.as_user_in_session(1000, 1000, &env, &argv).unwrap();
        let line: Vec<String> = line
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            line,
            [
                "/usr/bin/setpriv".to_string(),
                "--reuid=1000".into(),
                "--regid=1000".into(),
                "--init-groups".into(),
                "/usr/bin/env".into(),
                "-i".into(),
                "HOME=/home/me".into(),
                format!("XDG_RUNTIME_DIR={}", runtime.display()),
                format!(
                    "DBUS_SESSION_BUS_ADDRESS=unix:path={}",
                    runtime.join("bus").display()
                ),
                "/usr/bin/docker".into(),
                "version".into(),
            ]
        );

        // Not logged in: no session to point at.
        let line = dir.as_user_in_session(1001, 1001, &env, &argv).unwrap();
        assert!(
            !line
                .iter()
                .any(|a| a.to_string_lossy().starts_with("XDG_RUNTIME_DIR="))
        );
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
