use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A missing or corrupt index means no workspaces; each entry is re-verified
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
    // Renamed into place: a crash mid-write would otherwise leave a corrupt
    // index, which reads as no workspaces at all.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(index)?)
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("failed to write {}", path.display()))
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
