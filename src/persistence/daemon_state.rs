use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A small, single-writer bag of daemon-level settings that need to survive
/// `fghjd` restarting on its own (crash, reboot) — today just whether the
/// operator last asked for `daemon stop`, but expected to grow more fields
/// over time (see `DaemonControl` in `daemon.rs`). Plain JSON with
/// `#[serde(default)]` fields, same as `load_index`/`save_index`: there's
/// only ever one writer and no relational structure here, so a new field is
/// just a new struct field, no migration machinery needed.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    /// Set by `fghj daemon stop`, cleared by `fghj daemon start`. Checked at
    /// `fghjd` startup so a crash/reboot restart comes back idle instead of
    /// silently reactivating behind the operator's back.
    #[serde(default)]
    pub idle_requested: bool,
}

/// A missing or corrupt file just means "defaults" — there's nothing to
/// reconcile against, unlike the workspace index.
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
            },
        )
        .unwrap();
        assert!(
            !load_daemon_state(&path).idle_requested,
            "`daemon start` must clear the flag so future restarts come back active"
        );
    }
}
