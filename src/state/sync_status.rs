use serde::{Deserialize, Serialize};

/// Whether a container still matches what `.fghj.yaml` would produce now.
/// Informational only: nothing acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    /// No check yet, or the last one couldn't resolve the spec.
    #[default]
    Unknown,
    Synced,
    Drifted,
    /// The container runs but its node is gone from the graph (e.g. after a
    /// `git switch`). Nothing acts on it; the user decides.
    Orphaned,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_unknown() {
        assert_eq!(SyncStatus::default(), SyncStatus::Unknown);
    }

    #[test]
    fn variants_are_distinguishable() {
        assert_ne!(SyncStatus::Synced, SyncStatus::Drifted);
        assert_ne!(SyncStatus::Synced, SyncStatus::Unknown);
        assert_ne!(SyncStatus::Drifted, SyncStatus::Unknown);
    }

    #[test]
    fn orphaned_is_not_unknown() {
        assert_ne!(SyncStatus::Orphaned, SyncStatus::Unknown);
        assert_ne!(SyncStatus::Orphaned, SyncStatus::Synced);
        assert_ne!(SyncStatus::Orphaned, SyncStatus::Drifted);
    }

    #[test]
    fn serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&SyncStatus::Drifted).unwrap(),
            "\"drifted\""
        );
        assert_eq!(
            serde_json::to_string(&SyncStatus::Orphaned).unwrap(),
            "\"orphaned\""
        );
    }
}
