use std::collections::BTreeMap;

use super::name::Name;
use super::service::ServiceConfig;
use super::version::Version;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Deserialize, Clone)]
pub struct ComponentConfig {
    pub(crate) version: Version,
    #[serde(default)]
    pub(crate) include: BTreeMap<Name, Include>,
    pub(crate) services: BTreeMap<Name, ServiceConfig>,
    #[serde(default)]
    pub(crate) flows: BTreeMap<Name, Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum Include {
    Url(String),
    Detailed {
        repo: String,
        /// Read once, at clone time — see `concepts/branch-ownership-model.md`.
        #[serde(default)]
        default_branch: Option<String>,
    },
}

impl Include {
    pub(crate) fn repo(&self) -> &str {
        match self {
            Include::Url(repo) | Include::Detailed { repo, .. } => repo,
        }
    }

    pub(crate) fn default_branch(&self) -> Option<&str> {
        match self {
            Include::Url(_) => None,
            Include::Detailed { default_branch, .. } => default_branch.as_deref(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct BuildSecret {
    pub(crate) id: String,
    pub(crate) file: String,
}

#[derive(Debug, Clone)]
pub struct Build {
    pub(crate) context: String,
    pub(crate) dockerfile: String,
    pub(crate) dockerfile_inline: Option<String>,
    pub(crate) args: BTreeMap<String, String>,
    pub(crate) target: Option<String>,
    pub(crate) ssh: bool,
    pub(crate) secrets: Vec<BuildSecret>,
}

#[derive(Deserialize)]
struct BuildFull {
    #[serde(default = "default_context")]
    context: String,
    #[serde(default = "default_dockerfile")]
    dockerfile: String,
    #[serde(default)]
    dockerfile_inline: Option<String>,
    #[serde(default)]
    args: BTreeMap<String, String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    ssh: bool,
    #[serde(default)]
    secrets: Vec<BuildSecret>,
}

impl<'de> Deserialize<'de> for Build {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Context(String),
            Full(BuildFull),
        }
        let full = match Raw::deserialize(de)? {
            Raw::Context(context) => BuildFull {
                context,
                dockerfile: default_dockerfile(),
                dockerfile_inline: None,
                args: BTreeMap::new(),
                target: None,
                ssh: false,
                secrets: Vec::new(),
            },
            Raw::Full(full) => full,
        };
        Ok(Build {
            context: full.context,
            dockerfile: full.dockerfile,
            dockerfile_inline: full.dockerfile_inline,
            args: full.args,
            target: full.target,
            ssh: full.ssh,
            secrets: full.secrets,
        })
    }
}

pub fn default_context() -> String {
    ".".to_string()
}

pub fn default_dockerfile() -> String {
    "Dockerfile".to_string()
}

pub fn default_restart() -> String {
    "no".to_string()
}

/// Docker's own default; also used by stub nodes.
pub fn default_stop_grace_period() -> u64 {
    10
}

pub fn default_run_policy() -> String {
    "on_start".to_string()
}

/// Seconds here; converted to nanoseconds at the Docker boundary.
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
    pub(crate) fn to_pairs(&self) -> Vec<String> {
        match self {
            Environment::Map(m) => m.iter().map(|(k, v)| format!("{k}={v}")).collect(),
            Environment::List(l) => l.clone(),
        }
    }
}
