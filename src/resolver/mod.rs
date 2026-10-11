//! Turns a workspace of checked-out repos into the resolved node/edge graph.

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
