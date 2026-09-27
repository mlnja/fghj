//! Top-level `.fghj.yaml` shapes and the `#RunOptions` pieces shared by
//! services and backing dependencies.

use std::collections::BTreeMap;

use super::dependency::Dependency;
use super::name::Name;
use super::service::ServiceConfig;
use super::version::Version;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Clone)]
pub struct FlowConfig {
    #[allow(dead_code)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) service: Option<Name>,
    pub(crate) dependencies: Vec<Dependency>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ComponentConfig {
    /// Checked, not decoration — see [`super::version`] for why major is
    /// a barrier and minor is not.
    pub(crate) version: Version,
    pub(crate) services: BTreeMap<Name, ServiceConfig>,
    #[serde(default)]
    pub(crate) flows: BTreeMap<String, FlowConfig>,
}

/// One `--mount=type=secret` source — see `#BuildSecret` in
/// `schema/component.cue`, including why there is no `env` variant.
#[derive(Debug, Deserialize, Clone)]
pub struct BuildSecret {
    pub(crate) id: String,
    pub(crate) file: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Build {
    #[serde(default = "default_context")]
    pub(crate) context: String,
    #[serde(default = "default_dockerfile")]
    pub(crate) dockerfile: String,
    #[serde(default)]
    pub(crate) args: BTreeMap<String, String>,
    /// `docker build --target` — which stage of a multi-stage Dockerfile to
    /// build. `None` builds the final stage.
    #[serde(default)]
    pub(crate) target: Option<String>,
    /// Forward the workspace owner's ssh-agent into the build.
    #[serde(default)]
    pub(crate) ssh: bool,
    #[serde(default)]
    pub(crate) secrets: Vec<BuildSecret>,
}

pub fn default_context() -> String {
    ".".to_string()
}

pub fn default_dockerfile() -> String {
    "Dockerfile".to_string()
}

pub fn default_domain_scope() -> String {
    "run".to_string()
}

pub fn default_restart() -> String {
    "no".to_string()
}

/// Mirrors `#RunOptions.stop_grace_period`'s default, which is Docker's own
/// 10 seconds. Named rather than inlined because both config types need it
/// as a `serde(default)`, and a stub `Node` (which has no config to read)
/// has to land on the same number.
pub fn default_stop_grace_period() -> u64 {
    10
}

/// Mirrors `#Task.run`'s default. "on_start" re-runs a task on every start
/// and top-up; "once" runs it at most once per run. See the schema's own
/// doc comment for why the idempotent one is the default.
pub fn default_run_policy() -> String {
    "on_start".to_string()
}

/// Mirrors `#Healthcheck` in `schema/dependency.cue`. `interval`/`timeout`/
/// `start_period` are in seconds here — converted to the nanoseconds
/// Docker's API wants at the `docker::run_container` boundary.
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct Healthcheck {
    pub test: Vec<String>,
    #[serde(default)]
    pub interval: Option<u64>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub start_period: Option<u64>,
    #[serde(default)]
    pub retries: Option<u64>,
}

/// Mirrors `#Environment` in `schema/dependency.cue`: Compose accepts `environment`
/// as either a map of KEY: value or a list of "KEY=value" strings.
#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(untagged)]
pub enum Environment {
    Map(BTreeMap<String, String>),
    List(Vec<String>),
}

impl Default for Environment {
    fn default() -> Self {
        Environment::List(Vec::new())
    }
}

impl Environment {
    /// Normalizes into a list of "KEY=value" strings, suitable for `docker run -e`.
    pub(crate) fn to_pairs(&self) -> Vec<String> {
        match self {
            Environment::Map(m) => m.iter().map(|(k, v)| format!("{k}={v}")).collect(),
            Environment::List(l) => l.clone(),
        }
    }
}
