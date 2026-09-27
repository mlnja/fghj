//! The git facts read off a checkout: its remote, branch, and dirtiness.

use std::path::Path;
use std::process::Command;

/// Reads the `origin` remote URL and checked-out branch of a real git working
/// tree, if any — used so a downloaded node still carries the `repo`/`branch`
/// info the UI displays per node (see `Node::repo`/`Node::branch`), even
/// though resolution itself no longer needs it to find the node on disk.
pub fn git_remote_and_branch(dir: &Path) -> (Option<String>, Option<String>) {
    let repo = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let branch = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    (repo, branch)
}

/// The commit a checkout currently has checked out, as a full SHA.
///
/// This is what makes a new commit visible to drift detection. The image tag
/// fghj builds is `fghj/{id}:{branch}`, which is stable across commits — so
/// without this, committing (or pulling, or rebasing) on the same branch left
/// the drift hash byte-identical while the running container served a stale
/// image. See `runs::spec_hash` and `concepts/config-drift.md`.
///
/// `None` when the directory isn't a git working tree, or git can't be run at
/// all. Deliberately not "treated as changed" the way `git_status_dirty`
/// treats an unreadable status as dirty: an unknown *commit* is constant
/// across calls, so folding it into the hash as `None` is stable, whereas
/// inventing a value would report drift on every single tick.
pub fn git_head_sha(dir: &Path) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|sha| !sha.is_empty())
}

/// Whether a git working tree has uncommitted changes (or its status can't be
/// read at all — treated as dirty since we can't vouch for it being clean).
pub fn git_status_dirty(dir: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
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
        // The developer running these tests may well have `commit.gpgsign`
        // on globally; a test fixture must not try to reach a keyring.
        git(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("a"), "one").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-qm", "one"]);
    }

    /// The fact drift detection depends on: two commits on the *same branch*
    /// must give two different answers, because the image tag does not
    /// change between them.
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

    /// `None`, not a made-up value: an unknown commit has to be *stable*
    /// across calls, or every reconcile tick would report drift.
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
