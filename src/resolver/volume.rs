//! Volume mounts and the extra hostnames a service also answers on.

use super::config::default_domain_scope;
use super::name::Name;
use serde::{Deserialize, Serialize};

/// A bind mount or a named volume, on either a service or a backing
/// dependency. Mirrors `#Volume` in `schema/component.cue` — the untagged
/// shapes match its `{host,...}` vs `{name,scope,...}` disjunction directly.
/// A named volume's real Docker name is derived (never author-declared),
/// the same way a node's domain is — see `runs::derive_volume_name`, which
/// folds in the declaring node's id unless `shared` is set.
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
        #[serde(default = "default_domain_scope")]
        scope: String,
        container: String,
        #[serde(default)]
        read_only: bool,
        /// Opt in to the flat, workspace-global name — see `#Volume.shared`
        /// in `schema/component.cue`. Off by default: an unqualified volume
        /// namespace is the one place in the design where two configs
        /// written by teams who have never spoken silently point two
        /// database engines at one data directory.
        #[serde(default)]
        shared: bool,
    },
}

/// One `#HostAlias` entry: a bare hostname (exact match) or a hostname with
/// an explicit wildcard toggle (also matches every subdomain of it). Mirrors
/// `#HostAlias` in `schema/component.cue` — see its doc comment.
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
