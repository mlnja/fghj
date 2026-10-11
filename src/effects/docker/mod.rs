//! Docker convergence (`converge`) and observation (`observe`, `volumes`).
//! Drift is observed, never acted on.

pub mod converge;
pub mod observe;
pub mod volumes;

pub use converge::DockerConvergeEffect;
