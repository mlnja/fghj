//! `#BackingDependency` and the dependency edge kinds a component can declare.

use std::collections::BTreeMap;

use super::config::{Environment, Healthcheck, default_domain_scope, default_restart};
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
    #[serde(rename = "shared-backing")]
    SharedBacking {
        #[serde(default)]
        repo: Option<String>,
        service: Name,
        name: Name,
    },
}
