//! Top-level `.fghj.yaml` shapes and the `#RunOptions` pieces every
//! service shares.

use std::collections::BTreeMap;

use super::name::Name;
use super::service::ServiceConfig;
use super::version::Version;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Deserialize, Clone)]
pub struct ComponentConfig {
    /// Checked, not decoration — see [`super::version`] for why major is
    /// a barrier and minor is not.
    pub(crate) version: Version,
    /// Other repos this one uses, keyed by the alias this file refers to
    /// them by (`billing/pricing`). The only link between repos — see
    /// `concepts/flows-v2.md`.
    #[serde(default)]
    pub(crate) include: BTreeMap<Name, Include>,
    pub(crate) services: BTreeMap<Name, ServiceConfig>,
    /// Named start lists: each entry is one of this repo's services, one
    /// of its flows, an included repo's flow (`alias/flow`), or a whole
    /// included repo (`alias`).
    #[serde(default)]
    pub(crate) flows: BTreeMap<Name, Vec<String>>,
}

/// One `include:` entry: either the bare repo URL, or the URL plus the
/// branch to clone it at the first time.
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

/// One `--mount=type=secret` source — see `#BuildSecret` in
/// `schema/component.cue`, including why there is no `env` variant.
#[derive(Debug, Deserialize, Clone)]
pub struct BuildSecret {
    pub(crate) id: String,
    pub(crate) file: String,
}

/// Compose's `build`: either a bare context path (`build: .`) or the full
/// form.
#[derive(Debug, Clone)]
pub struct Build {
    pub(crate) context: String,
    pub(crate) dockerfile: String,
    /// The Dockerfile itself, for a context that has none — see
    /// `concepts/git-build-sources.md`.
    pub(crate) dockerfile_inline: Option<String>,
    pub(crate) args: BTreeMap<String, String>,
    /// `docker build --target` — which stage of a multi-stage Dockerfile to
    /// build. `None` builds the final stage.
    pub(crate) target: Option<String>,
    /// Forward the workspace owner's ssh-agent into the build.
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
