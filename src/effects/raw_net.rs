//! Fans every registered workspace's raw-zone containers into
//! `raw_net::reconcile`, as a `FannedInEffect`. Nothing else may call
//! `raw_net::reconcile` while this effect runs — two schedules touching the same `pf` state is the exact bug class documented
//! in `raw_net::macos`'s module doc.
//!
//! The projection itself lives in `state::query::raw_endpoints`, shared
//! with the `/daemon/net-status` telemetry endpoint, which needs the same
//! endpoint list for a completely different purpose (mapping a virtual IP
//! back to the domain it belongs to). This file is only the plumbing that
//! turns it into a converge loop.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::effects::FannedInEffect;
use crate::raw_net::{self, RawEndpoint};
use crate::state::{WorkspaceState, query};

/// The daemon-wide raw-net effect: aggregates every registered workspace's
/// running containers and reconciles the pf/virtual-IP NAT to match.
pub struct RawNetEffect;

impl FannedInEffect for RawNetEffect {
    type Snapshot = Vec<RawEndpoint>;

    fn extract(&self, states: &BTreeMap<String, Arc<WorkspaceState>>) -> Self::Snapshot {
        query::raw_endpoints(states)
    }

    fn converge(&mut self, snapshot: &Self::Snapshot) -> anyhow::Result<()> {
        raw_net::reconcile(snapshot)
    }
}
