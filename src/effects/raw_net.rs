//! //! Feeds every workspace's raw-zone endpoints into `raw_net::reconcile`. The
//! //! only caller of it: two writers of the same `pf` state race.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::effects::FannedInEffect;
use crate::raw_net::{self, RawEndpoint};
use crate::state::{WorkspaceState, query};

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
