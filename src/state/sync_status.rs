use serde::Serialize;

/// Whether a container's actual config still matches what `.fghj.yaml`
/// would produce right now — the reducer-state counterpart of
/// `runs::RunRegistry::config_drift`'s verdict. Purely informational:
/// nothing feeds a `Drifted` or `Orphaned` reading back into a reducer
/// decision (see the architecture plan's "Container drift policy:
/// observer-only by default" section, and
/// `effects::docker::converge`'s module doc) — these exist so the UI can
/// tell the user what happened and let *them* decide. That split is the
/// whole point: the observation must be representable before any policy
/// about it can exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    /// Nothing conclusive to say: either no drift check has run for this
    /// container yet, or re-resolving its spec failed on the last check.
    /// Note this no longer covers "the node is gone" — that is
    /// `Orphaned`, and separating the two is the reason this enum has a
    /// fourth variant. Conflating them meant a service that vanished from
    /// the graph (the branch-switch case in `concepts/AUDIT.md`'s B5) was
    /// indistinguishable from one nobody had inspected yet.
    #[default]
    Unknown,
    Synced,
    Drifted,
    /// The container is running, but its node no longer exists in the
    /// freshly-resolved graph — the `.fghj.yaml` that declared it is gone
    /// or no longer declares it. The usual cause is a `git switch` in a
    /// workspace checkout (fghj is a passive observer of branch state, per
    /// `concepts/branch-ownership-model.md`, so the graph changes under a
    /// live run); deleting a repo from the workspace does it too.
    ///
    /// Nothing in fghj acts on this. The container keeps running, keeps
    /// its route, and keeps its volumes — the user decides whether to stop
    /// it, exactly as with a crashed container.
    Orphaned,
}

impl From<Option<bool>> for SyncStatus {
    /// Widens the nullable bool SQLite's `synced` column stores back into
    /// the named states. `NULL` restores as `Unknown`, never `Orphaned`:
    /// whether a node still exists is a property of the *current* graph on
    /// disk, so a verdict from a previous `fghjd` lifetime would be a
    /// guess. The sync reconciler re-derives it on its next tick.
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
    /// kept next to its inverse so the round trip can't drift apart.
    /// Lossy for `Orphaned`, which stores as `NULL` alongside `Unknown`:
    /// the column's shape is deliberately unchanged so an older `fghjd`
    /// still reads this db (see `persistence::sqlite::runs::sync_to_column`),
    /// and re-deriving orphanhood on the next sync tick is both cheap and
    /// more correct than trusting a persisted answer about a graph that
    /// has since been re-read from disk.
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

    /// The whole reason `Orphaned` exists: "this node is gone from the
    /// graph" used to be spelled `Unknown`, i.e. the same value as "nobody
    /// has checked yet". Anything that can act on one must be able to tell
    /// them apart.
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

    /// `Orphaned` is the one variant the column can't hold — it stores as
    /// `NULL` and restores as `Unknown`, on purpose, because orphanhood is
    /// a fact about the graph currently on disk rather than about this
    /// container. Asserted so the lossiness is a decision, not a surprise.
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
