//! `#Service` — one entry under `services:`. Your own code (`build`), a
//! backing service (`image` only), or a task (something waits on it with
//! `service_completed_successfully`) all share this one shape, as they do in
//! Compose.

use std::collections::BTreeMap;

use super::config::{
    Build, Environment, Healthcheck, default_domain_scope, default_restart,
    default_stop_grace_period,
};
use super::dependency::DependsOn;
use super::port::BackingPorts;
use super::volume::{HostAliasConfig, VolumeMount};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct ServiceConfig {
    #[serde(default)]
    pub(crate) build: Option<Build>,
    /// A published image to run instead of building one. Exactly one of
    /// `build`/`image` is required: fghj names the images it builds itself,
    /// so Compose's "both" (tag the build) has nothing to mean here.
    #[serde(default)]
    pub(crate) image: Option<String>,
    /// A bare list of container ports, or a map of port to `#Port`.
    #[serde(default)]
    pub(crate) ports: BackingPorts,
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
    /// `None` means "not written": a service gets `"no"`, and a task must
    /// not set anything else — see `visit::check_task`.
    #[serde(default)]
    pub(crate) restart: Option<String>,
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
    pub(crate) platform: Option<String>,
    #[serde(default)]
    pub(crate) debug: Option<u16>,
    /// Tasks only: "on_start" (the default) or "once".
    #[serde(default)]
    pub(crate) run: Option<String>,
    #[serde(default)]
    pub(crate) depends_on: DependsOn,
}

impl ServiceConfig {
    pub(crate) fn restart_or_default(&self) -> String {
        self.restart.clone().unwrap_or_else(default_restart)
    }
}
