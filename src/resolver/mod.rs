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
//! - [`dependency`] — `depends_on`, Compose's syntax plus another repo's flow.
//! - [`port`], [`volume`] — declared ports, volume mounts, extra hostnames.
//!
//! The resolved output and the machinery that produces it:
//!
//! - [`graph`] — [`Node`], [`Edge`], [`Graph`].
//! - [`workspace_scan`] — finding and reading every `.fghj.yaml` on disk.
//! - [`git`] — the git facts read off a checkout (remote, branch, dirtiness).
//! - [`repo_url`] — git remote URL parsing and normalization.
//! - [`visit`] — the phases that turn components into nodes and edges.
//! - [`flows`] — flow expansion and each flow's start set.
//! - [`universe`] — [`resolve_universe`], the orchestrator over all of it.

pub mod build_source;
pub mod config;
pub mod cycles;
pub mod dependency;
pub mod flows;
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
pub mod volume;
pub mod warning;
pub mod workspace_scan;

#[cfg(test)]
mod tests;

pub use config::{Build, ComponentConfig, Environment, Healthcheck, Include};
pub use dependency::{Condition, DependsOn, DependsOnEntry};
pub use git::{git_head_sha, git_remote_and_branch, git_status_dirty};
pub use graph::{BuildSource, Edge, Graph, Node, NodeBuild, NodeBuildSecret, required_closure};
pub use name::Name;
pub use port::{BackingPorts, PortConfig};
pub use repo_url::{normalize_repo_url, repo_name_from_url};
pub use service::ServiceConfig;
pub use universe::{resolve_universe, resolve_universe_async};
pub use version::{SCHEMA_VERSION, Version};
pub use visit::ResolveCtx;
pub use volume::{HostAliasConfig, VolumeMount};
pub use warning::{Severity, Warning};
pub use workspace_scan::{ScannedWorkspace, read_component_file, scan_workspace};
