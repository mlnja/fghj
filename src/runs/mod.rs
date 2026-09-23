//! Run lifecycle: turning a resolved `Graph` into real Docker containers and
//! keeping the recorded state honest against what Docker actually reports.
//!
//! See `concepts/run-lifecycle-and-registry.md` for the design. The module is
//! split so each file holds one responsibility.
//!
//! Pure helpers, free of `RunRegistry` and of I/O:
//!
//! - [`naming`] — run ids and derived Docker volume names.
//! - [`domain`] — which `*.fghj.internal` zone a node answers on.
//! - [`fqdn_template`] — the `${FGHJ_SERVICE_FQDN}` expansion language.
//! - [`order`] — dependency-first start ordering.
//! - [`spec`] — the per-node launch spec and its drift hash.
//!
//! The shapes this module reads and writes — `RunState`, `ContainerInfo`,
//! `PortRoute`, `PendingAction` — live in [`crate::state`], which is where
//! the reducer, `persistence`, and the `/runs` JSON get them from too.
//! There is exactly one of each; this module used to define its own flatter
//! copies and translate at every boundary.
//!
//! I/O helpers:
//!
//! - [`route_table`] — the on-disk route table the sidecar reads.
//! - [`health`] — healthcheck polling.
//! - [`logs`] — container log capture and lookup.
//!
//! [`RunRegistry`] itself — the orchestrator. Its inherent `impl` is split
//! across these files by the job each group of methods does:
//!
//! - [`registry`] — the struct and its constructor.
//! - [`events`] — operator-facing narration of what an action is doing.
//! - [`observe`] — read-only reconciliation against real Docker state.
//! - [`lifecycle`] — per-node get/stop/restart/remove.
//! - [`sidecar`] — the per-run sidecar container.
//! - [`orchestrate`] — starting a whole run and bringing it up to date.
//! - [`node_spec`] — deriving one node's launch spec (image, binds, env).
//! - [`start_node`] — creating one node's container and its routes.

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
pub mod registry;
pub mod route_table;
pub mod sidecar;
pub mod spec;
pub mod start_node;

#[cfg(test)]
pub mod testing;

pub use domain::{DomainZone, derive_domain};
pub use logs::{container_name_for, logs_for_tail};
pub use naming::{DEFAULT_RUN_ID, resolve_run_id};
pub use order::topological_start_order;
pub use registry::RunRegistry;
