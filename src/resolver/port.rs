//! Declared ports, in both the map and the bare-list spelling.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A declared container port and its role. `primary` (at most one per node)
/// puts it at the node's own derived domain; `name` gives it an additional
/// nested domain `{name}.{node's domain}` — `runs::start_node` derives both
/// the same way it derives the node's own domain, so a named port can never
/// collide across runs/workspaces either. Neither set: still published to an
/// ephemeral localhost port, just with no `*.fghj.internal` name. Mirrors
/// `#Port` in `schema/component.cue`.
#[derive(Debug, Default, Deserialize, Clone, Serialize)]
pub struct PortConfig {
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub host_port: Option<u16>,
    /// When `primary` and/or `name` is set, also match every subdomain of
    /// this port's derived domain, not just the exact name — mirrors
    /// `#Port.wildcard` in `schema/component.cue`. No effect otherwise.
    #[serde(default)]
    pub wildcard: bool,
}

/// Mirrors `#BackingDependency.ports` in `schema/dependency.cue`: either a
/// bare list of port numbers (each implicitly non-primary, unnamed) or a map
/// of port number to `#Port` config, same shape `#Service.ports` always
/// uses — a backing dependency exposing more than one port with different
/// roles (minio's S3 API + console, grafana/prometheus's UI vs. write
/// endpoint) needs the same `primary`/`name` split a service does.
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
