//! //! Environment lifecycle: turns a resolved `Graph` into Docker containers.
//! //! See `concepts/run-lifecycle-and-registry.md`.

pub mod domain;
pub mod events;
pub mod fqdn_template;
pub mod health;
pub mod lifecycle;
pub mod logs;
pub mod naming;
pub mod node_spec;
pub mod observe;
pub mod orchestrate;
pub mod order;
pub mod progress;
pub mod registry;
pub mod route_table;
pub mod sidecar;
pub mod spec;
pub mod start_node;

#[cfg(test)]
pub mod testing;

pub use domain::{DomainZone, derive_domain};
pub use logs::{container_name_for, logs_for_tail};
pub use order::topological_start_order;
pub use registry::Orchestrator;
