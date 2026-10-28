//! Routes every workspace's `wildcard_hosts` suffixes to fghj's DNS server
//! through the OS resolver (`platform::DnsRouting`). Its only caller.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::dns;
use crate::effects::FannedInEffect;
use crate::state::{WorkspaceState, query};

pub struct DnsEffect {
    pub port: u16,
}

impl FannedInEffect for DnsEffect {
    type Snapshot = Vec<String>;

    fn extract(&self, states: &BTreeMap<String, Arc<WorkspaceState>>) -> Self::Snapshot {
        query::wildcard_suffixes(states)
    }

    fn converge(&mut self, snapshot: &Self::Snapshot) -> anyhow::Result<()> {
        dns::install_os_resolver_config(self.port, snapshot)
    }
}
