use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

/// Runs git as the owner of `dir`: fghjd is root, and git refuses repos owned
/// by someone else ("dubious ownership"). Not `-c safe.directory`, which would
/// have root honour a user-writable config that can name hooks and pagers.
fn git_in(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);

    if unsafe { libc::geteuid() } != 0 {
        return cmd;
    }
    let Ok(meta) = std::fs::metadata(dir) else {
        return cmd;
    };
    let (uid, gid) = (meta.uid(), meta.gid());
    if uid == 0 {
        return cmd;
    }

    cmd.uid(uid).gid(gid);
    // Otherwise git reads root's `HOME` and fails on its config.
    match crate::persistence::workspace_owner::home_dir_of(uid) {
        Some(home) => {
            cmd.env("HOME", home);
        }
        None => {
            cmd.env_remove("HOME");
        }
    }
    cmd
}

pub fn git_remote_and_branch(dir: &Path) -> (Option<String>, Option<String>) {
    let repo = git_in(dir)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let branch = git_in(dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    (repo, branch)
}

/// In the drift hash because the image tag `fghj/{id}:{branch}` doesn't change
/// between commits. `None` stays stable across calls, unlike guessing would.
pub fn git_head_sha(dir: &Path) -> Option<String> {
    git_in(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|sha| !sha.is_empty())
}

/// Unreadable status counts as dirty.
pub fn git_status_dirty(dir: &Path) -> bool {
    git_in(dir)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs")
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }

    fn repo_with_one_commit(dir: &Path) {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.email", "t@example.invalid"]);
        git(dir, &["config", "user.name", "t"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("a"), "one").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-qm", "one"]);
    }

    #[test]
    fn head_moves_with_a_commit_on_the_same_branch() {
        let tmp = tempfile::tempdir().unwrap();
        repo_with_one_commit(tmp.path());
        let first = git_head_sha(tmp.path()).expect("a committed repo has a HEAD");
        assert_eq!(first.len(), 40);

        std::fs::write(tmp.path().join("a"), "two").unwrap();
        git(tmp.path(), &["commit", "-qam", "two"]);

        assert_ne!(first, git_head_sha(tmp.path()).unwrap());
    }

    #[test]
    fn a_directory_that_is_not_a_git_tree_has_no_head() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(git_head_sha(tmp.path()).is_none());
        assert!(git_head_sha(tmp.path()).is_none());
    }

    #[test]
    fn an_edited_file_makes_the_tree_dirty_without_moving_head() {
        let tmp = tempfile::tempdir().unwrap();
        repo_with_one_commit(tmp.path());
        let head = git_head_sha(tmp.path()).unwrap();
        assert!(!git_status_dirty(tmp.path()));

        std::fs::write(tmp.path().join("a"), "edited").unwrap();
        assert!(git_status_dirty(tmp.path()));
        assert_eq!(head, git_head_sha(tmp.path()).unwrap());
    }
}
