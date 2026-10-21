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
    // Several reads of one repo at once otherwise race for `index.lock` to
    // refresh the index, and each loser rehashes the worktree instead.
    cmd.env("GIT_OPTIONAL_LOCKS", "0");

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

/// What the graph reads of one checkout.
#[derive(Debug, Default, Clone)]
pub struct RepoFacts {
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub dirty: bool,
}

/// One read per repo per resolve, the repos in parallel: it's four git calls
/// each, and a workspace has a dozen repos.
pub fn repo_facts<'a>(
    dirs: impl IntoIterator<Item = (&'a str, std::path::PathBuf)>,
) -> std::collections::HashMap<String, RepoFacts> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = dirs
            .into_iter()
            .map(|(local_path, dir)| {
                let handle = scope.spawn(move || {
                    let (repo, branch) = git_remote_and_branch(&dir);
                    RepoFacts {
                        repo,
                        branch,
                        head: git_head_sha(&dir),
                        dirty: git_status_dirty(&dir),
                    }
                });
                (local_path.to_string(), handle)
            })
            .collect();
        handles
            .into_iter()
            .map(|(local_path, handle)| (local_path, handle.join().unwrap_or_default()))
            .collect()
    })
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

/// The Repos tab's view of one checkout. Read on request, not part of the
/// universe: it costs three git calls per repo.
#[derive(Debug, Default, PartialEq, serde::Serialize)]
pub struct RepoStatus {
    pub branch: Option<String>,
    pub head: Option<String>,
    /// `origin/main` and the like; `None` when the branch tracks nothing.
    pub upstream: Option<String>,
    /// Commits on the branch the upstream lacks, and the reverse, as of the
    /// last fetch.
    pub ahead: u32,
    pub behind: u32,
    /// The first [`MAX_CHANGES`]; `changes_total` counts them all.
    pub changes: Vec<Change>,
    pub changes_total: usize,
    pub last_commit: Option<Commit>,
    /// When `FETCH_HEAD` was last written: a clone that was never fetched
    /// has none.
    pub fetched_at_ms: Option<u64>,
}

#[derive(Debug, PartialEq, serde::Serialize)]
pub struct Change {
    /// Git's two-letter code: index then worktree, `.` for unchanged, `??`
    /// for untracked.
    pub code: String,
    pub path: String,
}

#[derive(Debug, PartialEq, serde::Serialize)]
pub struct Commit {
    pub sha: String,
    pub subject: String,
    pub author: String,
    pub time_ms: u64,
}

pub const MAX_CHANGES: usize = 200;

/// `None` when `dir` isn't a readable git tree.
pub fn repo_status(dir: &Path) -> Option<RepoStatus> {
    let out = git_in(dir)
        .args(["status", "--porcelain=v2", "--branch"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let mut status = parse_porcelain_v2(&String::from_utf8_lossy(&out.stdout));
    status.last_commit = git_in(dir)
        .args(["log", "-1", "--format=%H%x1f%s%x1f%an%x1f%ct"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| parse_commit(&String::from_utf8_lossy(&o.stdout)));
    status.fetched_at_ms = git_in(dir)
        .args(["rev-parse", "--git-path", "FETCH_HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let path = dir.join(String::from_utf8_lossy(&o.stdout).trim());
            let modified = std::fs::metadata(path).ok()?.modified().ok()?;
            let ms = modified
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis();
            Some(ms as u64)
        });
    Some(status)
}

fn parse_porcelain_v2(text: &str) -> RepoStatus {
    let mut status = RepoStatus::default();
    for line in text.lines() {
        if let Some(header) = line.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" if value != "(initial)" => status.head = Some(value.to_string()),
                "branch.head" if value != "(detached)" => status.branch = Some(value.to_string()),
                "branch.upstream" => status.upstream = Some(value.to_string()),
                "branch.ab" => {
                    let mut counts = value
                        .split(' ')
                        .map(|n| n.trim_start_matches(['+', '-']).parse().unwrap_or(0));
                    status.ahead = counts.next().unwrap_or(0);
                    status.behind = counts.next().unwrap_or(0);
                }
                _ => {}
            }
            continue;
        }
        // Fields before the path: ordinary 8, renamed 9, unmerged 10.
        let change = match line.split_once(' ') {
            Some(("1", rest)) => rest.splitn(8, ' ').collect::<Vec<_>>(),
            Some(("2", rest)) => rest.splitn(9, ' ').collect::<Vec<_>>(),
            Some(("u", rest)) => rest.splitn(10, ' ').collect::<Vec<_>>(),
            Some(("?", path)) => vec!["??", path],
            _ => continue,
        };
        let (Some(code), Some(path)) = (change.first(), change.last()) else {
            continue;
        };
        status.changes_total += 1;
        if status.changes.len() < MAX_CHANGES {
            // A rename's path is `new<TAB>old`.
            let path = path.split('\t').next().unwrap_or(path);
            status.changes.push(Change {
                code: code.to_string(),
                path: path.to_string(),
            });
        }
    }
    status
}

fn parse_commit(text: &str) -> Option<Commit> {
    let mut fields = text.trim_end_matches('\n').split('\x1f');
    let sha = fields.next()?.to_string();
    let subject = fields.next()?.to_string();
    let author = fields.next()?.to_string();
    let time_ms = fields.next()?.parse::<u64>().ok()? * 1000;
    (!sha.is_empty()).then_some(Commit {
        sha,
        subject,
        author,
        time_ms,
    })
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

    #[test]
    fn porcelain_v2_gives_branch_upstream_and_changes() {
        let status = parse_porcelain_v2(
            "# branch.oid 1234abcd\n\
             # branch.head main\n\
             # branch.upstream origin/main\n\
             # branch.ab +2 -5\n\
             1 .M N... 100644 100644 100644 aaa bbb src/a file.rs\n\
             2 R. N... 100644 100644 100644 aaa bbb R100 new.rs\told.rs\n\
             u UU N... 100644 100644 100644 100644 aaa bbb ccc both.rs\n\
             ? notes.txt\n",
        );
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.head.as_deref(), Some("1234abcd"));
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.ahead, status.behind), (2, 5));
        let changes: Vec<(&str, &str)> = status
            .changes
            .iter()
            .map(|c| (c.code.as_str(), c.path.as_str()))
            .collect();
        assert_eq!(
            changes,
            vec![
                (".M", "src/a file.rs"),
                ("R.", "new.rs"),
                ("UU", "both.rs"),
                ("??", "notes.txt"),
            ]
        );
        assert_eq!(status.changes_total, 4);
    }

    #[test]
    fn a_detached_head_with_no_upstream_has_no_branch() {
        let status = parse_porcelain_v2("# branch.oid 1234abcd\n# branch.head (detached)\n");
        assert_eq!(status.branch, None);
        assert_eq!(status.upstream, None);
        assert_eq!((status.ahead, status.behind), (0, 0));
    }

    #[test]
    fn a_real_checkout_reports_its_last_commit_and_edits() {
        let tmp = tempfile::tempdir().unwrap();
        repo_with_one_commit(tmp.path());
        std::fs::write(tmp.path().join("a"), "edited").unwrap();
        std::fs::write(tmp.path().join("b"), "new").unwrap();

        let status = repo_status(tmp.path()).unwrap();
        assert_eq!(status.last_commit.as_ref().unwrap().subject, "one");
        assert_eq!(status.head, git_head_sha(tmp.path()));
        assert_eq!(status.changes_total, 2);
        assert_eq!(status.fetched_at_ms, None);
    }
}
