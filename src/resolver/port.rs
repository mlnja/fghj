use std::collections::BTreeMap;

use super::name::Name;
use serde::{Deserialize, Serialize};

/// `primary` puts it at the node's domain; `name` at `{name}.{node's domain}`.
/// Neither: published to an ephemeral port with no name.
#[derive(Debug, Default, Deserialize, Clone, Serialize)]
pub struct PortConfig {
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub name: Option<Name>,
    #[serde(default)]
    pub host_port: Option<u16>,
    #[serde(default)]
    pub wildcard: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum BackingPorts {
    List(Vec<String>),
    Map(BTreeMap<String, PortConfig>),
}

impl Default for BackingPorts {
    fn default() -> Self {
        BackingPorts::List(Vec::new())
    }
}

impl BackingPorts {
    pub(crate) fn into_map(self) -> BTreeMap<String, PortConfig> {
        match self {
            BackingPorts::List(l) => l.into_iter().map(|p| (p, PortConfig::default())).collect(),
            BackingPorts::Map(m) => m,
        }
    }
}
