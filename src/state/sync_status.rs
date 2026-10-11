use serde::Serialize;

/// Whether a container still matches what `.fghj.yaml` would produce now.
/// Informational only: nothing acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
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

impl From<Option<bool>> for SyncStatus {
    /// `NULL` restores as `Unknown`: orphanhood is re-derived from the current graph.
    fn from(synced: Option<bool>) -> Self {
        match synced {
            Some(true) => SyncStatus::Synced,
            Some(false) => SyncStatus::Drifted,
            None => SyncStatus::Unknown,
        }
    }
}

impl From<SyncStatus> for Option<bool> {
    /// Lossy: `Orphaned` stores as `NULL`.
    fn from(sync: SyncStatus) -> Self {
        match sync {
            SyncStatus::Synced => Some(true),
            SyncStatus::Drifted => Some(false),
            SyncStatus::Unknown | SyncStatus::Orphaned => None,
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
    fn orphaned_is_not_unknown() {
        assert_ne!(SyncStatus::Orphaned, SyncStatus::Unknown);
        assert_ne!(SyncStatus::Orphaned, SyncStatus::Synced);
        assert_ne!(SyncStatus::Orphaned, SyncStatus::Drifted);
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
    fn orphaned_narrows_to_null_and_restores_as_unknown() {
        assert_eq!(Option::<bool>::from(SyncStatus::Orphaned), None);
        assert_eq!(
            SyncStatus::from(Option::<bool>::from(SyncStatus::Orphaned)),
            SyncStatus::Unknown
        );
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
