//! //! Per-node progress out of a create/top-up, so a crash mid-create doesn't
//! //! leave running containers nothing recorded, and the UI fills in as nodes
//! //! come up. A channel, not an `ActorHandle`: `orchestrator/` doesn't know
//! //! about actors.

use tokio::sync::mpsc;

use crate::state::{ContainerInfo, PendingAction};

#[derive(Debug, Clone)]
pub enum RunProgress {
    /// Sent before the Docker work so the node reads "starting…" during a build.
    /// `None` clears the mark after a failure.
    Working {
        node_id: String,
        action: Option<PendingAction>,
    },
    Done(Box<NodeDone>),
}

#[derive(Debug, Clone)]
pub struct NodeDone {
    pub network: String,
    pub sidecar_container_name: String,
    pub sidecar_ip: Option<String>,
    pub info: ContainerInfo,
}

/// Unbounded: the producer is a slow sequential loop.
pub type ProgressSink = mpsc::UnboundedSender<RunProgress>;

/// Ignores a closed channel: nobody listening is no reason to abort a create.
pub(crate) fn report(sink: Option<&ProgressSink>, progress: RunProgress) {
    if let Some(sink) = sink {
        let _ = sink.send(progress);
    }
}
