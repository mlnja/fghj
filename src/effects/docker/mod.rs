//! Docker convergence effects. `converge` (migration phase 4) drives real
//! start/stop/delete calls off `ContainerInfo::pending_action`. `observe`
//! (migration phase 7) is the always-on drift reporter — see its module
//! doc for why it piggybacks on `daemon::spawn_reconciler`'s existing tick
//! rather than polling Docker a second time. `volumes` is `observe`'s
//! volume-discovery half. `policy` holds the (currently unused)
//! `DockerHealPolicy` switch that would let observed drift feed back into
//! `converge` — see the plan's "Container drift policy: observer-only by
//! default" section.

pub mod converge;
pub mod observe;
pub mod policy;
pub mod volumes;

pub use converge::DockerConvergeEffect;
pub use policy::DockerHealPolicy;
