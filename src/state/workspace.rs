use std::path::PathBuf;

use serde::Serialize;

use super::env::EnvState;
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
    /// The workspace's one environment — `None` until something first
    /// creates it.
    pub env: Option<EnvState>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workspace_state_has_no_env_and_no_owner() {
        let state = WorkspaceState::default();
        assert!(state.env.is_none());
        assert!(state.owner.is_none());
    }
}
