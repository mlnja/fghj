//! Linux sessions through systemd-logind, and the per-user runtime directory
//! (`/run/user/<uid>`) where session agents and rootless Docker keep their
//! sockets.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

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

type Loginctl = Box<dyn Fn(&[&str]) -> Option<String> + Send + Sync>;

pub(super) struct LogindSession {
    /// `loginctl` with these arguments: its stdout, or `None` if it failed.
    /// Not logind's files under `/run/systemd`, which it marks private.
    loginctl: Loginctl,
    run_user: PathBuf,
}

impl LogindSession {
    pub(super) fn system() -> Self {
        Self {
            loginctl: Box::new(run_loginctl),
            run_user: PathBuf::from("/run/user"),
        }
    }

    /// Whoever is logged in at the screen.
    fn seat_user(&self) -> Option<u32> {
        let session = (self.loginctl)(&["show-seat", "seat0", "-p", "ActiveSession", "--value"])?;
        let session = session.trim();
        if session.is_empty() {
            return None;
        }
        let uid = (self.loginctl)(&["show-session", session, "-p", "User", "--value"])?;
        uid.trim().parse().ok().filter(|uid| *uid != 0)
    }

    /// A headless machine has no seat: the one person logged in, over ssh or
    /// otherwise, if there is exactly one.
    fn only_user(&self) -> Option<u32> {
        let sessions = (self.loginctl)(&["list-sessions", "--no-legend"])?;
        let mut uids = BTreeSet::new();
        for id in sessions.lines().filter_map(|l| l.split_whitespace().next()) {
            let Some(props) = (self.loginctl)(&["show-session", id, "-p", "User", "-p", "Class"])
            else {
                continue;
            };
            if property(&props, "Class") != Some("user") {
                continue;
            }
            if let Some(uid) = property(&props, "User").and_then(|u| u.parse::<u32>().ok())
                && uid != 0
            {
                uids.insert(uid);
            }
        }
        match uids.len() {
            1 => uids.pop_first(),
            _ => None,
        }
    }
}

impl Session for LogindSession {
    fn state_root(&self) -> PathBuf {
        PathBuf::from("/var/lib/fghjd")
    }

    fn console_uid(&self) -> Option<u32> {
        self.seat_user().or_else(|| self.only_user())
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

    /// Rootless Docker's, which its setup tool doesn't always register as a
    /// context.
    fn docker_sockets(&self, uid: u32) -> Vec<String> {
        let sock = self.run_user.join(uid.to_string()).join("docker.sock");
        vec![format!("unix://{}", sock.display())]
    }
}

fn run_loginctl(args: &[&str]) -> Option<String> {
    let output = Command::new("loginctl").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `Key=value` lines, as `loginctl show-*` prints them without `--value`.
fn property<'a>(output: &'a str, key: &str) -> Option<&'a str> {
    output
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers from a table keyed by the joined arguments.
    fn session(answers: &[(&str, &str)], run_user: &Path) -> LogindSession {
        let answers: Vec<(String, String)> = answers
            .iter()
            .map(|(args, out)| (args.to_string(), out.to_string()))
            .collect();
        LogindSession {
            loginctl: Box::new(move |args| {
                let key = args.join(" ");
                answers
                    .iter()
                    .find(|(args, _)| *args == key)
                    .map(|(_, out)| out.clone())
            }),
            run_user: run_user.to_path_buf(),
        }
    }

    const SEAT: &str = "show-seat seat0 -p ActiveSession --value";

    #[test]
    fn the_seats_active_session_names_the_console_user() {
        let s = session(
            &[(SEAT, "2\n"), ("show-session 2 -p User --value", "1000\n")],
            Path::new("/run/user"),
        );
        assert_eq!(s.console_uid(), Some(1000));
    }

    #[test]
    fn without_a_seat_the_only_logged_in_user_stands_in() {
        // systemd 249 pads the columns; 255 adds more of them.
        let s = session(
            &[
                (SEAT, "\n"),
                (
                    "list-sessions --no-legend",
                    "     3 1000 dev        pts/0\nc1 0 root\n",
                ),
                ("show-session 3 -p User -p Class", "User=1000\nClass=user\n"),
                ("show-session c1 -p User -p Class", "User=0\nClass=user\n"),
            ],
            Path::new("/run/user"),
        );
        assert_eq!(s.console_uid(), Some(1000));
    }

    #[test]
    fn two_users_logged_in_without_a_seat_is_ambiguous() {
        let s = session(
            &[
                (
                    "list-sessions --no-legend",
                    "3 1000 a\n4 1001 b\n5 1000 a\n",
                ),
                ("show-session 3 -p User -p Class", "User=1000\nClass=user\n"),
                ("show-session 4 -p User -p Class", "User=1001\nClass=user\n"),
                (
                    "show-session 5 -p User -p Class",
                    "User=1000\nClass=manager\n",
                ),
            ],
            Path::new("/run/user"),
        );
        assert_eq!(s.console_uid(), None);
    }

    #[test]
    fn a_root_session_at_the_seat_is_nobody_to_act_as() {
        let s = session(
            &[(SEAT, "1\n"), ("show-session 1 -p User --value", "0\n")],
            Path::new("/run/user"),
        );
        assert_eq!(s.console_uid(), None);
    }

    #[test]
    fn no_loginctl_means_no_console_user() {
        assert_eq!(session(&[], Path::new("/run/user")).console_uid(), None);
    }

    #[test]
    fn the_first_live_agent_in_the_runtime_dir_wins() {
        let s = session(&[], Path::new("/run/user"));
        let live = |p: &Path| {
            p == Path::new("/run/user/1000/openssh_agent")
                || p == Path::new("/run/user/1000/gnupg/S.gpg-agent.ssh")
        };
        assert_eq!(
            s.session_ssh_agent(1000, &live).as_deref(),
            Some("/run/user/1000/openssh_agent")
        );
        assert_eq!(s.session_ssh_agent(1001, &live), None);
    }

    #[test]
    fn only_the_known_agent_sockets_count_as_the_session_agent() {
        let s = session(&[], Path::new("/run/user"));
        assert!(s.is_session_agent("/run/user/1000/gcr/ssh"));
        assert!(s.is_session_agent("/run/user/1000/gnupg/S.gpg-agent.ssh"));
        assert!(!s.is_session_agent("/run/user/1000/gcr/ssh/extra"));
        assert!(!s.is_session_agent("/run/user/me/gcr/ssh"));
        assert!(!s.is_session_agent("/home/me/.1password/agent.sock"));
    }

    #[test]
    fn rootless_docker_lives_in_the_runtime_dir() {
        let s = session(&[], Path::new("/run/user"));
        assert_eq!(
            s.docker_sockets(1000),
            vec!["unix:///run/user/1000/docker.sock".to_string()]
        );
    }

    #[test]
    fn property_reads_key_value_lines_exactly() {
        let out = "UserName=x\nUser=1000\nClass=user\n";
        assert_eq!(property(out, "User"), Some("1000"));
        assert_eq!(property(out, "Missing"), None);
    }
}
