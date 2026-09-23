use serde::Serialize;

/// Whether a container's actual config still matches what `.fghj.yaml`
/// would produce right now — the reducer-state counterpart of
/// `runs::RunRegistry::refresh_sync_status`'s `Option<bool>` result
/// (`src/runs.rs`), spelled out as three explicit states instead of one
/// nullable bool. Purely informational: nothing in this migration ever
/// feeds a `Drifted` reading back into a reducer decision (see the
/// architecture plan's "Container drift policy: observer-only by default"
/// section) — it only exists for the UI's "Desired state" vs. "Actual
/// state" indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    /// No drift check has run yet for this container, *or* its node no
    /// longer exists in the current `.fghj.yaml` — deliberately conflated,
    /// exactly as `refresh_sync_status`'s doc comment explains its own
    /// `None` result already conflates them: in both cases there is
    /// nothing meaningful left to compare a stored config hash against.
    #[default]
    Unknown,
    Synced,
    Drifted,
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
    fn serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&SyncStatus::Drifted).unwrap(),
            "\"drifted\""
        );
    }
}
