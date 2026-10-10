//! Incremental progress reporting out of a whole-run create/top-up.
//!
//! `ensure_running` bring nodes up one at a time and used to
//! persist the whole `RunState` to SQLite after each one, so that a daemon
//! that died partway through still knew about the containers that had
//! already come up. Persistence is now derived from published state
//! (`effects::persist`), which only learns about a create when it settles —
//! so without something here, a crash mid-create would leave running
//! containers nothing had recorded. That is exactly the `Orphaned` state
//! the observer works to make visible, manufactured by fghj itself.
//!
//! Reporting each node as it comes up restores that guarantee and makes the
//! create incremental everywhere else too: the UI fills in node by node
//! instead of staying empty until the last one is healthy — and, with
//! `RunProgress::Working`, shows which node is being worked on right now.
//!
//! Deliberately a channel rather than an `ActorHandle`: `runs/` drives
//! Docker and should not know that actors, actions, or reducers exist. The
//! effect on the other end does the translating.

use tokio::sync::mpsc;

use crate::state::{ContainerInfo, PendingAction};

/// One step of a create/top-up/switch, as it happens.
#[derive(Debug, Clone)]
pub enum RunProgress {
    /// About to start (or, for a flow switch, stop) `node_id` — reported
    /// before the Docker work rather than after, so the node reads
    /// "starting…"/"stopping…" for the minutes a build can take instead of
    /// sitting unchanged while the environment rejects every other action as
    /// already in flight. `None` takes the mark back off a node whose work
    /// failed, so it doesn't read as still in progress for the rest of the
    /// create.
    Working {
        run_id: String,
        node_id: String,
        action: Option<PendingAction>,
    },
    /// A node's state once its work is done.
    Done(Box<NodeDone>),
}

/// One node having come up (or been stopped), plus the run-level facts
/// that are only known once the run's network and sidecar exist. Carrying
/// them on every report (rather than once up front) keeps the receiver from
/// having to sequence two different kinds of message.
#[derive(Debug, Clone)]
pub struct NodeDone {
    pub run_id: String,
    pub network: String,
    pub sidecar_container_name: String,
    pub sidecar_ip: Option<String>,
    pub info: ContainerInfo,
}

/// Where a create reports its per-node progress. Unbounded because the
/// producer is a sequential loop doing Docker work between sends — it
/// cannot outrun a consumer that only dispatches an action per message,
/// and blocking the create to apply backpressure would be worse than
/// buffering a handful of reports.
pub type ProgressSink = mpsc::UnboundedSender<RunProgress>;

/// Reports one node, ignoring a closed channel: the receiver going away
/// means nobody is recording progress any more, which is not a reason to
/// abandon a create that is otherwise going fine.
pub(crate) fn report(sink: Option<&ProgressSink>, progress: RunProgress) {
    if let Some(sink) = sink {
        let _ = sink.send(progress);
    }
}
