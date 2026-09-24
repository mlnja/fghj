//! Turns a workspace of checked-out repos into the resolved node/edge graph
//! everything else works from.
//!
//! See `concepts/node-identity-and-domains.md`, `concepts/flat-workspace-model.md`
//! and `concepts/fog-of-war-visibility.md` for the design. The module is split
//! so each file holds one responsibility.
//!
//! The parsed `.fghj.yaml` shapes — these mirror `schema/*.cue`:
//!
//! - [`config`] — the file's top-level shapes and the shared `#RunOptions` parts.
//! - [`service`] — `#Service`.
//! - [`dependency`] — `#BackingDependency` and the dependency edge kinds.
//! - [`port`], [`volume`] — declared ports, volume mounts, extra hostnames.
//!
//! The resolved output and the machinery that produces it:
//!
//! - [`graph`] — [`Node`], [`Edge`], [`Graph`].
//! - [`workspace_scan`] — finding and reading every `.fghj.yaml` on disk.
//! - [`git`] — the git facts read off a checkout (remote, branch, dirtiness).
//! - [`repo_url`] — git remote URL parsing and normalization.
//! - [`visit`] — the traversal that walks components into nodes and edges.
//! - [`universe`] — [`resolve_universe`], the orchestrator over all of it.

pub mod config;
pub mod cycles;
pub mod dependency;
pub mod git;
pub mod graph;
pub mod name;
pub mod port;
pub mod repo_url;
pub mod service;
pub mod uniqueness;
pub mod universe;
pub mod validate;
pub mod version;
pub mod visit;
pub mod visit_dependency;
pub mod volume;
pub mod warning;
pub mod workspace_scan;

#[cfg(test)]
mod tests;

pub use config::{Build, ComponentConfig, Environment, FlowConfig, Healthcheck};
pub use dependency::{BackingDependencyConfig, Dependency};
pub use git::{git_remote_and_branch, git_status_dirty};
pub use graph::{Edge, Graph, Node, NodeBuild};
pub use name::Name;
pub use port::{BackingPorts, PortConfig};
pub use repo_url::{normalize_repo_url, repo_name_from_url};
pub use service::ServiceConfig;
pub use universe::resolve_universe;
pub use version::{SCHEMA_VERSION, Version};
pub use visit::ResolveCtx;
pub use volume::{HostAliasConfig, VolumeMount};
pub use warning::{Severity, Warning};
pub use workspace_scan::{ScannedWorkspace, read_component_file, scan_workspace};
