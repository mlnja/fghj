use serde::Serialize;

/// Whether a container's actual config still matches what `.fghj.yaml`
/// would produce right now — the reducer-state counterpart of
/// `runs::RunRegistry::config_drift`'s `Option<bool>` verdict, spelled out
/// as three explicit states instead of one nullable bool. Purely informational: nothing in this migration ever
/// feeds a `Drifted` reading back into a reducer decision (see the
/// architecture plan's "Container drift policy: observer-only by default"
/// section) — it only exists for the UI's "Desired state" vs. "Actual
/// state" indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    /// No drift check has run yet for this container, *or* its node no
    /// longer exists in the current `.fghj.yaml` — deliberately conflated,
    /// exactly as `RunRegistry::config_drift`'s doc explains its own
    /// `None` result already conflates them: in both cases there is
    /// nothing meaningful left to compare a stored config hash against.
    #[default]
    Unknown,
    Synced,
    Drifted,
}

impl From<Option<bool>> for SyncStatus {
    /// Widens `runs::DriftReport`'s nullable bool — the shape the actual
    /// hash comparison produces — into the three named states. One of the
    /// two places the two spellings meet; see the inverse below.
    fn from(synced: Option<bool>) -> Self {
        match synced {
            Some(true) => SyncStatus::Synced,
            Some(false) => SyncStatus::Drifted,
            None => SyncStatus::Unknown,
        }
    }
}

impl From<SyncStatus> for Option<bool> {
    /// Narrows back to the nullable bool SQLite's `synced` column stores —
    /// kept next to its inverse so the round trip can't drift apart, and
    /// lossless in both directions since `Unknown` is exactly `NULL`.
    fn from(sync: SyncStatus) -> Self {
        match sync {
            SyncStatus::Synced => Some(true),
            SyncStatus::Drifted => Some(false),
            SyncStatus::Unknown => None,
        }
    }
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
    fn widens_every_nullable_bool() {
        assert_eq!(SyncStatus::from(Some(true)), SyncStatus::Synced);
        assert_eq!(SyncStatus::from(Some(false)), SyncStatus::Drifted);
        assert_eq!(SyncStatus::from(None), SyncStatus::Unknown);
    }

    #[test]
    fn round_trips_through_the_nullable_bool_column() {
        for status in [SyncStatus::Synced, SyncStatus::Drifted, SyncStatus::Unknown] {
            assert_eq!(SyncStatus::from(Option::<bool>::from(status)), status);
        }
    }

    #[test]
    fn serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&SyncStatus::Drifted).unwrap(),
            "\"drifted\""
        );
    }
}
