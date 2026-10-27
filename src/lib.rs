use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub mod action;
pub mod activity;
pub mod actor;
pub mod buildx;
pub mod daemon;
pub mod daemon_log;
pub mod dns;
pub mod docker;
pub mod dockerignore;
pub mod doctor;
pub mod downloads;
pub mod effects;
pub mod hosts_file;
pub mod orchestrator;
pub mod persistence;
pub mod platform;
pub mod raw_net;
pub mod reducer;
pub mod resolver;
pub mod server;
pub mod sidecar_image;
pub mod state;
pub mod supervisor;
pub mod uninstall;
pub mod util;
pub mod web;

/// `owner`: who to clone as, since root `fghjd` has no credentials. `None`
/// when already the right user.
pub fn resolve_workspace(
    entry: Option<String>,
    workspace: Option<PathBuf>,
    owner: Option<&persistence::WorkspaceOwner>,
) -> Result<PathBuf> {
    let workspace = workspace.unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&workspace)
        .with_context(|| format!("failed to create workspace dir {}", workspace.display()))?;

    if let Some(url) = entry {
        let local_path = resolver::repo_name_from_url(&url);
        let dest = workspace.join(&local_path);
        if !dest.exists() {
            let mut cmd = Command::new("git");
            cmd.args(["clone", "--quiet"]).arg(&url).arg(&dest);
            if let Some(owner) = owner {
                owner.apply_to_command(&mut cmd);
            }
            persistence::harden_git_ssh(&mut cmd);
            let output = cmd
                .output()
                .with_context(|| format!("failed to run git clone for {url}"))?;
            if !output.status.success() {
                bail!(
                    "git clone failed for {url}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
    }

    Ok(workspace)
}

/// Deepest match wins: an old index can still hold a nested pair, and order of
/// `candidates` must not decide. Component-wise, so `/w/app` doesn't capture
/// `/w/app-legacy`. Paths must be canonical.
pub fn enclosing_workspace(cwd: &Path, candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .filter(|candidate| cwd.starts_with(candidate))
        .max_by_key(|candidate| candidate.components().count())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enclosing_workspace_matches_the_workspace_root_itself() {
        let ws = PathBuf::from("/w/aikido");
        assert_eq!(
            enclosing_workspace(Path::new("/w/aikido"), std::slice::from_ref(&ws)),
            Some(ws)
        );
    }

    #[test]
    fn enclosing_workspace_matches_from_deep_inside_a_repo() {
        let ws = PathBuf::from("/w/aikido");
        assert_eq!(
            enclosing_workspace(
                Path::new("/w/aikido/aikifactory/internal/config"),
                std::slice::from_ref(&ws)
            ),
            Some(ws)
        );
    }

    #[test]
    fn enclosing_workspace_prefers_the_deepest_of_several() {
        let outer = PathBuf::from("/w");
        let inner = PathBuf::from("/w/aikido");
        let cwd = Path::new("/w/aikido/aikifactory");

        assert_eq!(
            enclosing_workspace(cwd, &[outer.clone(), inner.clone()]),
            Some(inner.clone())
        );
        assert_eq!(
            enclosing_workspace(cwd, &[inner.clone(), outer]),
            Some(inner)
        );
    }

    #[test]
    fn enclosing_workspace_does_not_match_a_sibling_with_a_shared_prefix() {
        let ws = PathBuf::from("/w/app");
        assert_eq!(
            enclosing_workspace(Path::new("/w/app-legacy/src"), &[ws]),
            None
        );
    }

    #[test]
    fn enclosing_workspace_is_none_when_outside_every_workspace() {
        let candidates = vec![PathBuf::from("/w/aikido"), PathBuf::from("/w/other")];
        assert_eq!(
            enclosing_workspace(Path::new("/tmp/scratch"), &candidates),
            None
        );
        assert_eq!(enclosing_workspace(Path::new("/tmp/scratch"), &[]), None);
    }

    #[test]
    fn resolve_workspace_creates_dir_without_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("nested").join("workspace");

        let result = resolve_workspace(None, Some(target.clone()), None).unwrap();

        assert_eq!(result, target);
        assert!(target.is_dir());
    }

    #[test]
    fn resolve_workspace_defaults_to_current_dir() {
        let result = resolve_workspace(None, None, None).unwrap();
        assert_eq!(result, PathBuf::from("."));
    }
}
