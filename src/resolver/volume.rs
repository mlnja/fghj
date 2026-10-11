use super::name::Name;
use serde::{Deserialize, Serialize};

/// A named volume's Docker name is derived (`orchestrator::derive_volume_name`).
#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(untagged)]
pub enum VolumeMount {
    Bind {
        host: String,
        container: String,
        #[serde(default)]
        read_only: bool,
    },
    Named {
        name: Name,
        container: String,
        #[serde(default)]
        read_only: bool,
        /// Off by default: otherwise two unrelated configs can point two databases at
        /// one data directory.
        #[serde(default)]
        shared: bool,
    },
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum HostAliasConfig {
    Bare(String),
    Detailed {
        host: String,
        #[serde(default)]
        wildcard: bool,
    },
}

impl HostAliasConfig {
    pub(crate) fn host(&self) -> &str {
        match self {
            HostAliasConfig::Bare(host) => host,
            HostAliasConfig::Detailed { host, .. } => host,
        }
    }

    pub(crate) fn wildcard(&self) -> bool {
        matches!(self, HostAliasConfig::Detailed { wildcard: true, .. })
    }
}
