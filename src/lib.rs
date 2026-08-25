use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

pub mod ca;
pub mod daemon;
pub mod dns;
pub mod docker;
pub mod downloads;
pub mod proxy;
pub mod resolver;
pub mod runs;
pub mod server;
pub mod store;

/// Resolves the workspace directory, cloning `entry` into it by convention if
/// given and not already present.
///
/// `owner` drops the clone's privileges back to the real user who ran
/// `fghj wire` (see [`store::WorkspaceOwner`]) — `fghjd` runs as root and has
/// no SSH credentials of its own for a private remote. Pass `None` when
/// already running as the correct user (e.g. the plain `fghj graph` CLI).
pub fn resolve_workspace(
    entry: Option<String>,
    workspace: Option<PathBuf>,
    owner: Option<&store::WorkspaceOwner>,
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
            store::harden_git_ssh(&mut cmd);
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

#[cfg(test)]
mod tests {
    use super::*;

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
