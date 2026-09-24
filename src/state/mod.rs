//! The canonical, reducer-owned shape of a workspace's in-memory state —
//! see `workspace::WorkspaceState` for the root type and the architecture
//! plan (rosy-soaring-teapot.md) for why this exists. Every type here is
//! data-only: no I/O, no async, nothing that can fail. Wired into the
//! running daemon via `action.rs`/`reducer/`/`actor.rs`/`registry.rs`/
//! `effects/`, and serialized straight to the frontend by
//! `daemon::get_runs`/`post_runs` and the per-node lifecycle handlers —
//! these types *are* the HTTP contract, there is no view layer in between.

pub mod container;
pub mod query;
pub mod run;
pub mod sync_status;
pub mod volume;
pub mod workspace;

pub use container::{ContainerDesired, ContainerInfo, ContainerObserved, PendingAction, PortRoute};
pub use run::{RunCreateError, RunSpec, RunState};
pub use sync_status::SyncStatus;
pub use volume::{VolumeDesired, VolumeInfo, VolumeObserved};
pub use workspace::WorkspaceState;
