use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Daemon-level settings that survive a restart. Plain JSON: one writer, so a
/// new field needs no migration.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    #[serde(default)]
    pub idle_requested: bool,
    /// The `unix://…` socket the last `fghj wire` (or `doctor --fix`) said its
    /// user's Docker answers on. `None` until the first one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_host: Option<String>,
}

/// A missing or corrupt file means defaults.
pub fn load_daemon_state(path: &Path) -> DaemonState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_daemon_state(path: &Path, state: &DaemonState) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    std::fs::write(path, serde_json::to_string_pretty(state)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_state_round_trips_and_defaults_to_active() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon-state.json");

        let defaulted = load_daemon_state(&path);
        assert!(
            !defaulted.idle_requested,
            "a freshly-started fghjd with no prior `daemon stop` must default to active"
        );

        save_daemon_state(
            &path,
            &DaemonState {
                idle_requested: true,
                ..DaemonState::default()
            },
        )
        .unwrap();
        assert!(
            load_daemon_state(&path).idle_requested,
            "`daemon stop` must persist so a crash/reboot restart doesn't silently reactivate"
        );

        save_daemon_state(
            &path,
            &DaemonState {
                idle_requested: false,
                ..DaemonState::default()
            },
        )
        .unwrap();
        assert!(
            !load_daemon_state(&path).idle_requested,
            "`daemon start` must clear the flag so future restarts come back active"
        );
    }
}
