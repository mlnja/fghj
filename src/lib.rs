use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

pub mod daemon;
pub mod docker;
pub mod downloads;
pub mod resolver;
pub mod runs;
pub mod server;
pub mod store;

/// Resolves the workspace directory, cloning `entry` into it by convention if
/// given and not already present.
pub fn resolve_workspace(entry: Option<String>, workspace: Option<PathBuf>) -> Result<PathBuf> {
    let workspace = workspace.unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&workspace)
        .with_context(|| format!("failed to create workspace dir {}", workspace.display()))?;

    if let Some(url) = entry {
        let local_path = resolver::repo_name_from_url(&url);
        let dest = workspace.join(&local_path);
        if !dest.exists() {
            let status = Command::new("git")
                .args(["clone", "--quiet"])
                .arg(&url)
                .arg(&dest)
                .status()
                .with_context(|| format!("failed to run git clone for {url}"))?;
            if !status.success() {
                bail!("git clone failed for {url}");
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

        let result = resolve_workspace(None, Some(target.clone())).unwrap();

        assert_eq!(result, target);
        assert!(target.is_dir());
    }

    #[test]
    fn resolve_workspace_defaults_to_current_dir() {
        let result = resolve_workspace(None, None).unwrap();
        assert_eq!(result, PathBuf::from("."));
    }
}
