use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use super::run::RunState;
use crate::persistence::WorkspaceOwner;

/// The single canonical, in-memory source of truth for one workspace —
/// everything every effect converges reality toward, and the only thing a
/// reducer is ever allowed to produce (see `reducer::reduce`). One of
/// these lives behind an `actor::ActorHandle` per registered workspace
/// (see `actor.rs`, `registry.rs`). Not to be confused with
/// `server::WorkspaceState`, the bundle of Docker-facing machinery
/// (`WorkspaceDb`, `RunRegistry`, `DownloadRegistry`) that carries out
/// what this one says.
///
/// Does not derive `PartialEq`: `WorkspaceOwner` (`persistence::workspace_owner`) doesn't
/// either, and nothing needs whole-struct equality on
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
