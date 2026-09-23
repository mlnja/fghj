use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use super::run::RunState;
use crate::persistence::WorkspaceOwner;

/// The single canonical, in-memory source of truth for one workspace —
/// everything every effect converges reality toward, and the only thing a
/// reducer is ever allowed to produce (see `reducer::reduce`). One of
/// these lives behind an `actor::ActorHandle` per registered workspace
/// (see `actor.rs`, `registry.rs`), replacing today's `server::WorkspaceState`
/// (path + `WorkspaceDb` + `RunRegistry` + `DownloadRegistry` bundle) —
/// that type keeps its name and shape unchanged for now (this phase must
/// not touch it); the two are expected to converge once migration phase 4+
/// re-points HTTP handlers at this one instead.
///
/// Does not derive `PartialEq`: `WorkspaceOwner` (`persistence::workspace_owner`) doesn't
/// either, and nothing in this phase needs whole-struct equality on
/// `WorkspaceState` itself — every effect's `Snapshot` type (see
/// `effects::Effect`) is expected to be a small, independently-`PartialEq`
/// projection of this struct, never this struct wholesale.
#[derive(Debug, Clone, Serialize, Default)]
pub struct WorkspaceState {
    pub path: PathBuf,
    pub owner: Option<WorkspaceOwner>,
    pub runs: BTreeMap<String, RunState>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workspace_state_has_no_runs_and_no_owner() {
        let state = WorkspaceState::default();
        assert!(state.runs.is_empty());
        assert!(state.owner.is_none());
    }
}
