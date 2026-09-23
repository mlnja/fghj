use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

// `fghjd` owns many workspaces, each of which owns its own durable state
// under `<workspace>/.fghj/` (see `sqlite::WorkspaceDb`). This file is just
// the id -> workspace-root pointer list `fghjd` reads on startup to find
// them all again — restarting the daemon (crash, reboot, upgrade) shouldn't
// forget which workspaces were wired.

/// A missing or corrupt index just means "no workspaces known yet", not a
/// startup failure — every entry is independently re-verified against disk
/// by `WorkspaceRegistry::load` anyway.
pub fn load_index(path: &Path) -> HashMap<String, PathBuf> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_index(path: &Path, index: &HashMap<String, PathBuf>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    std::fs::write(path, serde_json::to_string_pretty(index)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workspaces.json");

        assert!(load_index(&path).is_empty());

        let mut index = HashMap::new();
        index.insert("ws-abc".to_string(), PathBuf::from("/some/workspace"));
        save_index(&path, &index).unwrap();

        let loaded = load_index(&path);
        assert_eq!(
            loaded.get("ws-abc"),
            Some(&PathBuf::from("/some/workspace"))
        );
    }
}
