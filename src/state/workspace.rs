use std::path::PathBuf;

use serde::Serialize;

use super::env::EnvState;
use crate::persistence::WorkspaceOwner;

/// The workspace's single source of truth, owned by its actor and changed only
/// by the reducer. Not `server::WorkspaceState`, the machinery that carries it
/// out.
#[derive(Debug, Clone, Serialize, Default)]
pub struct WorkspaceState {
    pub path: PathBuf,
    pub owner: Option<WorkspaceOwner>,
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
