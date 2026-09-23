//! Fans every registered workspace's running containers' `wildcard_hosts`
//! suffixes into `dns::install_os_resolver_config` — the DNS half of the
//! architecture plan's (rosy-soaring-teapot.md) "dns + hosts_file effects"
//! migration step, following the same `FannedInEffect` idiom
//! `effects::raw_net::RawNetEffect` already established for the raw-net
//! slice. `daemon::spawn_reconciler` must never also call
//! `dns::install_os_resolver_config` directly once this effect is spawned —
//! two writers of the same `/etc/resolver` directory would just race each
//! other to reach the same end state.
//!
//! This is only the *write* side of DNS: which zones the OS routes to fghj's
//! server. Actually *answering* a query is a live read path, not a converge
//! loop, and lives in `daemon::routing`'s `dns::ZoneSource` impl — but both
//! now read the same zone list from `state::query::wildcard_suffixes`, so
//! the set of zones routed here can't drift from the set answered there.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::dns;
use crate::effects::FannedInEffect;
use crate::state::{WorkspaceState, query};

/// The daemon-wide DNS-routing effect: aggregates every registered
/// workspace's running containers' wildcard-host suffixes and syncs the OS
/// resolver config (macOS `/etc/resolver`) so each one routes to this DNS
/// server at `port`.
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
