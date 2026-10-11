//! The reducer-owned workspace state. Data only; serialized straight to the
//! UI, so these types are the HTTP contract.

pub mod container;
pub mod env;
pub mod query;
pub mod sync_status;
#[cfg(test)]
pub mod testing;
pub mod volume;
pub mod workspace;

pub use container::{
    ContainerDesired, ContainerInfo, ContainerObserved, ContainerSource, NodeCondition,
    PendingAction, PortRoute,
};
pub use env::{EnvCreateError, EnvSpec, EnvState};
pub use sync_status::SyncStatus;
pub use volume::{VolumeDesired, VolumeInfo, VolumeObserved};
pub use workspace::WorkspaceState;
