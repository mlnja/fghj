//! Finding and reading every `.fghj.yaml` on disk in the workspace.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use super::config::ComponentConfig;
use anyhow::{Context, Result};

pub fn read_component_file(path: &Path) -> Result<ComponentConfig> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_yaml::from_str(&contents)
        .with_context(|| format!("failed to parse {} as a component config", path.display()))
}

/// Reads every immediate subdirectory of `workspace` that contains an
/// `.fghj.yaml`, keyed by its local folder name (the convention-based repo id).
pub fn scan_workspace(workspace: &Path) -> Result<BTreeMap<String, ComponentConfig>> {
    let mut out = BTreeMap::new();
    if !workspace.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(workspace)
        .with_context(|| format!("failed to read workspace dir {}", workspace.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let config_path = path.join(".fghj.yaml");
        if !config_path.exists() {
            continue;
        }
        let local_path = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let component = read_component_file(&config_path)?;
        out.insert(local_path, component);
    }
    Ok(out)
}
