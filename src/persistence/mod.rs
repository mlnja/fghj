//! Durable state: per-workspace SQLite, the JSON index and daemon state, and
//! the workspace owner. Environment state is written only by `effects::persist`
//! and read once, at boot, by `rehydrate`. Logs and events are read and written
//! directly.

pub mod daemon_state;
pub mod index;
pub mod rehydrate;
pub mod sqlite;
pub mod workspace_owner;

mod paths;

pub use daemon_state::{DaemonState, load_daemon_state, save_daemon_state};
pub use index::{load_index, save_index};
pub use paths::{default_index_path, default_state_path, fghjd_root};
pub use rehydrate::rehydrate;
pub use sqlite::{EventEntry, LogGeneration, LogLine, WorkspaceDb};
pub use workspace_owner::{WorkspaceOwner, harden_git_ssh};
