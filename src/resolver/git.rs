//! The git facts read off a checkout: its remote, branch, and dirtiness.

use std::ffi::CStr;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

/// A `git -C <dir>` invocation that runs as the user who **owns** `dir`,
/// rather than as whoever is asking.
///
/// `fghjd` runs as root (it binds 80/443, installs a CA, writes
/// `/etc/resolver` — see `concepts/preflight-checks.md`), while the checkouts
/// it reads belong to the developer. Git refuses to operate on a repository
/// owned by somebody else since 2.35.2 ("detected dubious ownership"), exiting
/// non-zero, which silently turned every read below into its failure case: no
/// remote, no branch, no commit, and — because `git_status_dirty` cannot
/// vouch for a tree it failed to read — *every* node reported permanently
/// dirty. The daemon and the CLI resolved the same workspace into different
/// graphs, which is the kind of bug that looks like a UI problem for a while.
///
/// Dropping to the owner is the fix rather than `-c safe.directory`: that flag
/// would leave root running git against a config file an unprivileged user can
/// write, and git config can name a pager, a hook and a filter to execute. The
/// repo's owner is the right identity to read the repo with.
///
/// No-ops unless we are actually root and the directory belongs to someone
/// else — `uid()` on a `Command` would otherwise just make the spawn fail.
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
    // Without this, git inherits root's `HOME` and dies trying to read a
    // `/var/root/.gitconfig` the dropped uid cannot open — which would
    // reintroduce the very failure this function exists to remove.
    match home_dir_of(uid) {
        Some(home) => {
            cmd.env("HOME", home);
        }
        None => {
            cmd.env_remove("HOME");
        }
    }
    cmd
}

/// A uid's home directory, from the passwd database.
///
/// `getpwuid` rather than the `HOME` already in the environment: the whole
/// point is that the environment belongs to root and the uid does not.
fn home_dir_of(uid: u32) -> Option<String> {
    // SAFETY: `getpwuid` returns a pointer into a static buffer owned by libc,
    // which is read (and copied out of) before any other libc call that could
    // overwrite it. A null return means "no such user", handled below.
    unsafe {
        let pw = libc::getpwuid(uid as libc::uid_t);
        if pw.is_null() || (*pw).pw_dir.is_null() {
            return None;
        }
        CStr::from_ptr((*pw).pw_dir)
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

/// Reads the `origin` remote URL and checked-out branch of a real git working
/// tree, if any — used so a downloaded node still carries the `repo`/`branch`
/// info the UI displays per node (see `Node::repo`/`Node::branch`), even
/// though resolution itself no longer needs it to find the node on disk.
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
    git_in(dir)
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
