//! Everything that reads or writes durable state outside process memory:
//! per-workspace SQLite (`sqlite`), the two plain-JSON side files
//! (`index`, `daemon_state`), and the captured workspace-owner identity
//! (`workspace_owner`). SQLite/JSON are write-through and
//! read-once-at-boot — nothing reads them live during normal operation,
//! except `logs`/`events` (`sqlite::logs`/`sqlite::events`), a deliberate,
//! narrow exception: unbounded append-only telemetry nothing ever branches
//! a decision on, so they stay direct-write/direct-read.
//!
//! Environment state has exactly one writer and one reader.
//! `effects::persist` writes whatever the workspace actor publishes, so
//! the database can never hold a view the reducer didn't produce.
//! `rehydrate::rehydrate` reads it once, when `server::WorkspaceState` is
//! built, reconciling each container against live Docker and pruning dead
//! ones; the result seeds the actor
//! (`daemon::WorkspaceRegistry::wire_actor`).
//!
//! `index.json` (`index`) and `daemon-state.json` (`daemon_state`) are
//! daemon-level infrastructure rather than reducer state, written through
//! at the point of the relevant mutation (workspace
//! registration/deregistration; `daemon start`/`stop`).

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
