//! `#Service` — a component this workspace builds and runs from source.

use std::collections::BTreeMap;

use super::config::{Build, Environment, Healthcheck, default_domain_scope, default_restart};
use super::dependency::Dependency;
use super::port::PortConfig;
use super::volume::{HostAliasConfig, VolumeMount};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct ServiceConfig {
    #[serde(default)]
    pub(crate) build: Option<Build>,
    #[serde(default)]
    pub(crate) ports: BTreeMap<String, PortConfig>,
    #[serde(default = "default_domain_scope")]
    pub(crate) domain_scope: String,
    #[serde(default)]
    pub(crate) environment: Environment,
    #[serde(default)]
    pub(crate) env_file: Vec<String>,
    #[serde(default)]
    pub(crate) command: Vec<String>,
    #[serde(default)]
    pub(crate) volumes: Vec<VolumeMount>,
    #[serde(default)]
    pub(crate) additional_hosts: Vec<HostAliasConfig>,
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
    #[serde(default)]
    pub(crate) platform: Option<String>,
    #[serde(default)]
    pub(crate) dependencies: Vec<Dependency>,
}
