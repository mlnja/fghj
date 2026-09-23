//! The canonical, reducer-owned shape of a workspace's in-memory state —
//! see `workspace::WorkspaceState` for the root type and the architecture
//! plan (rosy-soaring-teapot.md) for why this exists. Every type here is
//! data-only: no I/O, no async, nothing that can fail. Wired into the
//! running daemon via `action.rs`/`reducer/`/`actor.rs`/`registry.rs`/
//! `effects/`, and served to the frontend (through `run_view`'s shim onto
//! the old JSON shape) by `daemon::get_runs`/`post_runs` and the per-node
//! lifecycle handlers.

pub mod container;
pub mod run;
pub mod sync_status;
pub mod volume;
pub mod workspace;

pub use container::{ContainerDesired, ContainerInfo, ContainerObserved, PendingAction, PortRoute};
pub use run::{RunSpec, RunState};
pub use sync_status::SyncStatus;
pub use volume::{VolumeDesired, VolumeInfo, VolumeObserved};
pub use workspace::WorkspaceState;
