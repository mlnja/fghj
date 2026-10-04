//! `#BackingDependency` and the dependency edge kinds a component can declare.

use std::collections::BTreeMap;

use super::config::{
    Environment, Healthcheck, default_domain_scope, default_restart, default_run_policy,
    default_stop_grace_period,
};
use super::name::Name;
use super::port::BackingPorts;
use super::volume::VolumeMount;
use serde::Deserialize;

/// The fields of a `Dependency::Backing` — pulled out into its own struct
/// (behind a `Box` at the use site) rather than inlined as a large struct
/// variant, since `Dependency::Service`/`SharedBacking` are tiny by
/// comparison and clippy's `large_enum_variant` flags the size gap
/// otherwise.
#[derive(Debug, Deserialize, Clone)]
pub struct BackingDependencyConfig {
    pub(crate) name: Name,
    pub(crate) image: String,
    #[serde(default)]
    pub(crate) environment: Environment,
    #[serde(default)]
    pub(crate) ports: BackingPorts,
    #[serde(default = "default_domain_scope")]
    pub(crate) domain_scope: String,
    #[serde(default)]
    pub(crate) command: Vec<String>,
    #[serde(default)]
    pub(crate) volumes: Vec<VolumeMount>,
    #[serde(default)]
    pub(crate) platform: Option<String>,
    #[serde(default)]
    pub(crate) env_file: Vec<String>,
    #[serde(default = "default_restart")]
    pub(crate) restart: String,
    #[serde(default)]
    pub(crate) stop_signal: Option<String>,
    #[serde(default = "default_stop_grace_period")]
    pub(crate) stop_grace_period: u64,
    #[serde(default)]
    pub(crate) user: Option<String>,
    #[serde(default)]
    pub(crate) working_dir: Option<String>,
    #[serde(default)]
    pub(crate) labels: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) cap_add: Vec<String>,
    #[serde(default)]
    pub(crate) cap_drop: Vec<String>,
    #[serde(default)]
    pub(crate) privileged: bool,
    #[serde(default)]
    pub(crate) extra_hosts: Vec<String>,
    #[serde(default)]
    pub(crate) healthcheck: Option<Healthcheck>,
    #[serde(default)]
    pub(crate) debug: Option<u16>,
}

/// The fields of a `Dependency::Task` — `#Task` in
/// `schema/dependency.cue`. Boxed at the use site for the same
/// `large_enum_variant` reason as `BackingDependencyConfig`.
///
/// Deliberately has no `restart` and no `healthcheck` field, where
/// `BackingDependencyConfig` has both. A task's completion predicate is its
/// exit code, and a restart policy on a container whose purpose is to exit
/// would restart it forever — so rather than accept the fields and ignore
/// them, they are absent from the type that decides what a task can say.
#[derive(Debug, Deserialize, Clone)]
pub struct TaskConfig {
    pub(crate) name: Name,
    /// Omitted means "the owning service's own built image" — resolved in
    /// `visit_dependency`, which is where the owner's `build` is reachable.
    #[serde(default)]
    pub(crate) image: Option<String>,
    /// A task *is* its command; an empty one is a blocking warning rather
    /// than a container that re-runs the image's own long-running `CMD` and
    /// never exits.
    #[serde(default)]
    pub(crate) command: Vec<String>,
    #[serde(default)]
    pub(crate) environment: Environment,
    #[serde(default)]
    pub(crate) volumes: Vec<VolumeMount>,
    /// Sibling dependencies of the *same owning service* to order after —
    /// see `#Task.after`.
    #[serde(default)]
    pub(crate) after: Vec<Name>,
    #[serde(default = "default_run_policy")]
    pub(crate) run: String,
    #[serde(default)]
    pub(crate) platform: Option<String>,
    #[serde(default)]
    pub(crate) env_file: Vec<String>,
    #[serde(default)]
    pub(crate) user: Option<String>,
    #[serde(default)]
    pub(crate) working_dir: Option<String>,
    #[serde(default)]
    pub(crate) labels: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) cap_add: Vec<String>,
    #[serde(default)]
    pub(crate) cap_drop: Vec<String>,
    #[serde(default)]
    pub(crate) privileged: bool,
    #[serde(default)]
    pub(crate) extra_hosts: Vec<String>,
    /// A task gets these even though it has no `restart` and no
    /// `healthcheck`: a task that is still running when the run is torn down
    /// is stopped like anything else, and a long migration is exactly the
    /// thing you do not want SIGKILLed halfway through.
    #[serde(default)]
    pub(crate) stop_signal: Option<String>,
    #[serde(default = "default_stop_grace_period")]
    pub(crate) stop_grace_period: u64,
    #[serde(default)]
    pub(crate) debug: Option<u16>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "kind")]
pub enum Dependency {
    #[serde(rename = "service")]
    Service {
        #[serde(default)]
        repo: Option<String>,
        #[serde(default)]
        default_branch: Option<String>,
        #[serde(default)]
        services: Vec<Name>,
    },
    #[serde(rename = "backing")]
    Backing(Box<BackingDependencyConfig>),
    #[serde(rename = "task")]
    Task(Box<TaskConfig>),
    #[serde(rename = "shared-backing")]
    SharedBacking {
        #[serde(default)]
        repo: Option<String>,
        service: Name,
        name: Name,
    },
}
