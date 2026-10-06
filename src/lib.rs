use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub mod action;
pub mod actor;
pub mod buildx;
pub mod daemon;
pub mod daemon_log;
pub mod dns;
pub mod docker;
pub mod doctor;
pub mod downloads;
pub mod effects;
pub mod hosts_file;
pub mod persistence;
pub mod raw_net;
pub mod reducer;
pub mod registry;
pub mod resolver;
pub mod runs;
pub mod server;
pub mod sidecar_image;
pub mod state;
pub mod supervisor;
pub mod uninstall;
pub mod util;
pub mod web;

/// Resolves the workspace directory, cloning `entry` into it by convention if
/// given and not already present.
///
/// `owner` drops the clone's privileges back to the real user who ran
/// `fghj wire` (see [`persistence::WorkspaceOwner`]) — `fghjd` runs as root
/// and has no SSH credentials of its own for a private remote. Pass `None`
/// when already running as the correct user (e.g. the plain `fghj graph`
/// CLI).
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

/// The workspace `cwd` sits inside, chosen from `candidates` (the paths
/// `GET /workspaces` reports as wired).
///
/// Exists so `fghj graph|wire|exec` can be run from anywhere inside a
/// workspace — including deep inside one of its repos — instead of only from
/// the workspace root. Without it, an omitted `--workspace` meant literally
/// `.`, so running `fghj exec` from inside `aikido/aikifactory/internal/`
/// looked for a workspace *there* and failed, which is not what anyone
/// means.
///
/// **Deepest** match wins, not first. `daemon::registry::nesting_conflict`
/// refuses to wire overlapping workspaces, so a nested pair should not
/// arise — but an index written by an older `fghjd` can still contain one,
/// and `load_from` warns about that rather than silently de-wiring a
/// workspace the operator may have runs in. For that case the inner
/// directory is the more specific answer, the same most-specific-match rule
/// `query::resolve_route` uses for hostnames, and iteration order of
/// `candidates` must not decide it.
///
/// Comparison is `Path::starts_with`, which is component-wise: a workspace
/// at `/w/app` does not capture a `cwd` of `/w/app-legacy`, the way a naive
/// string prefix would. Callers are responsible for passing canonical
/// paths — a symlinked `cwd` and a real `candidate` are different strings
/// and won't match.
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

    /// The whole point: any depth below the workspace resolves to it.
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

    /// Nested workspaces: the inner one is the more specific answer, and
    /// which one is listed first must not matter.
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

    /// Component-wise, not string-prefix: `/w/app-legacy` is not inside
    /// `/w/app`. A `starts_with` on strings would say it is.
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
