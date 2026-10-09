//! A git URL as a build context: `<url>#<ref>:<subdir>`, Compose's spelling.
//! The service stays the declaring repo's; only the code comes from the URL,
//! cloned into `.fghj/sources/` by pull. See `concepts/git-build-sources.md`.

use std::path::Path;

use super::git::{git_head_sha, git_status_dirty};
use super::graph::BuildSource;

/// Where every source is cloned, relative to the workspace.
pub const SOURCES_DIR: &str = ".fghj/sources";

/// A `build.context` that names a remote rather than a folder of the repo.
#[derive(Debug, PartialEq, Eq)]
pub struct RemoteContext {
    pub url: String,
    pub reference: Option<String>,
    pub subdir: Option<String>,
}

/// Whether `context` is a git URL. Anything else is a path in the repo.
pub fn is_remote(context: &str) -> bool {
    ["https://", "http://", "ssh://", "git@", "file://"]
        .iter()
        .any(|p| context.starts_with(p))
}

/// Splits `<url>#<ref>:<subdir>`, or `None` for a context that isn't a URL.
/// The fragment is split on its first `:`, so the colon of `git@host:org/x`
/// is never mistaken for a subdir.
pub fn parse_remote(context: &str) -> Option<RemoteContext> {
    if !is_remote(context) {
        return None;
    }
    let (url, fragment) = match context.split_once('#') {
        Some((url, fragment)) => (url, fragment),
        None => (context, ""),
    };
    let (reference, subdir) = match fragment.split_once(':') {
        Some((reference, subdir)) => (reference, subdir),
        None => (fragment, ""),
    };
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_string());
    Some(RemoteContext {
        url: url.to_string(),
        reference: non_empty(reference),
        subdir: non_empty(subdir.trim_matches('/')),
    })
}

/// The workspace-relative folder a source is cloned into:
/// `.fghj/sources/<name>@<ref>`, or `<name>` without a ref. A `/` in a branch
/// name becomes `-`, so the clone is always one folder deep.
pub fn source_path(url: &str, reference: Option<&str>) -> String {
    let name = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(url)
        .trim_end_matches(".git");
    match reference {
        Some(r) => format!("{SOURCES_DIR}/{name}@{}", r.replace('/', "-")),
        None => format!("{SOURCES_DIR}/{name}"),
    }
}

/// Whether `reference` can only be a commit: `git clone --branch` takes
/// branches and tags, so a commit is cloned whole and then checked out.
pub fn looks_like_commit(reference: &str) -> bool {
    (7..=40).contains(&reference.len()) && reference.chars().all(|c| c.is_ascii_hexdigit())
}

/// A subdir that stays inside the clone: relative, and no `..`.
pub fn subdir_is_contained(subdir: &str) -> bool {
    !subdir.starts_with('/') && !subdir.split('/').any(|seg| seg == "..")
}

impl BuildSource {
    /// The source `remote` names, with what's on disk at its clone path.
    pub fn read(workspace: &Path, remote: &RemoteContext) -> Self {
        let path = source_path(&remote.url, remote.reference.as_deref());
        let dir = workspace.join(&path);
        let downloaded = dir.join(".git").exists();
        BuildSource {
            url: remote.url.clone(),
            reference: remote.reference.clone(),
            head: downloaded.then(|| git_head_sha(&dir)).flatten(),
            dirty: downloaded && git_status_dirty(&dir),
            downloaded,
            path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_with_ref_and_subdir() {
        assert_eq!(
            parse_remote("https://github.com/acme/geocoder.git#v2.3.1:server").unwrap(),
            RemoteContext {
                url: "https://github.com/acme/geocoder.git".into(),
                reference: Some("v2.3.1".into()),
                subdir: Some("server".into()),
            }
        );
    }

    #[test]
    fn an_scp_style_url_keeps_its_colon() {
        let remote = parse_remote("git@github.com:acme/geocoder.git#main").unwrap();
        assert_eq!(remote.url, "git@github.com:acme/geocoder.git");
        assert_eq!(remote.reference.as_deref(), Some("main"));
        assert_eq!(remote.subdir, None);
    }

    #[test]
    fn a_subdir_without_a_ref_uses_the_default_branch() {
        let remote = parse_remote("https://example.com/x.git#:cmd/x/").unwrap();
        assert_eq!(remote.reference, None);
        assert_eq!(remote.subdir.as_deref(), Some("cmd/x"));
    }

    #[test]
    fn a_path_is_not_remote() {
        assert_eq!(parse_remote("./docker"), None);
        assert_eq!(parse_remote("."), None);
    }

    #[test]
    fn the_clone_path_is_named_after_the_url_and_ref() {
        assert_eq!(
            source_path("https://github.com/acme/geocoder.git", Some("v2.3.1")),
            ".fghj/sources/geocoder@v2.3.1"
        );
        assert_eq!(
            source_path("git@github.com:geocoder.git", None),
            ".fghj/sources/geocoder"
        );
        assert_eq!(
            source_path("https://x/y.git", Some("feature/z")),
            ".fghj/sources/y@feature-z"
        );
    }

    #[test]
    fn commits_are_told_apart_from_branches_and_tags() {
        assert!(looks_like_commit("3f2a9c1"));
        assert!(looks_like_commit(
            "3f2a9c1d8e7b6a5f4e3d2c1b0a9f8e7d6c5b4a3f"
        ));
        assert!(!looks_like_commit("main"));
        assert!(!looks_like_commit("v2.3.1"));
        assert!(!looks_like_commit("cafe"));
    }

    #[test]
    fn a_subdir_cannot_leave_the_clone() {
        assert!(subdir_is_contained("server"));
        assert!(!subdir_is_contained("../other"));
        assert!(!subdir_is_contained("/etc"));
    }
}
