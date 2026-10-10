//! Docker convergence effects. `converge` drives real start/stop/delete
//! calls off `ContainerInfo::pending_action`, and whole-environment
//! creates off `RunState::pending_create`. `observe` is the always-on
//! drift reporter — see its module
//! doc for why it piggybacks on `daemon::spawn_reconciler`'s existing tick
//! rather than polling Docker a second time. `volumes` is `observe`'s
//! volume-discovery half. `policy` holds the (currently unused)
//! `DockerHealPolicy` switch that would let observed drift feed back into
//! `converge`; by default drift is only observed, never acted on.

pub mod converge;
pub mod observe;
pub mod policy;
pub mod volumes;

pub use converge::DockerConvergeEffect;
pub use policy::DockerHealPolicy;
