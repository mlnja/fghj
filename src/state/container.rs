use std::collections::BTreeMap;

use serde::Serialize;

use super::sync_status::SyncStatus;

/// A `*.fghj.internal` name this container answers to, and the `127.0.0.1`
/// port Docker actually published its backing container-side port on —
/// carried over field-for-field from `runs::PortRoute` (`src/runs.rs`),
/// which this type will replace once `RunState`/`ContainerInfo` fold into
/// `WorkspaceState` for real (migration phase 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortRoute {
    pub domain: String,
    pub host_port: u16,
    /// When set, `domain` is a wildcard suffix rather than an exact name —
    /// see `runs::PortRoute::wildcard`'s doc for the exact matching rule
    /// this drives in `WorkspaceRegistry::resolve_route`.
    pub wildcard: bool,
    /// The container-side port (a key into `ContainerObserved::ports`) this
    /// route was derived from.
    pub container_port: String,
    /// Whether `domain` is eligible for a cert from fghj's local CA — see
    /// `runs::PortRoute::https`'s doc for the exact rule.
    pub https: bool,
}

/// Which start/stop/delete action is currently in flight for a container —
/// carried over unchanged from `runs::PendingAction`. This is the field
/// the reducer's in-flight dedup checks (`container.pending_action.is_some()`
/// → `ActionRejected::AlreadyInFlight`), and the field
/// `Action::ContainerActionSettled` clears once the effect actually doing
/// the Docker call finishes — see `reducer::run` and `reducer::observation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingAction {
    Starting,
    Stopping,
    Removing,
}

impl std::fmt::Display for PendingAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PendingAction::Starting => write!(f, "starting"),
            PendingAction::Stopping => write!(f, "stopping"),
            PendingAction::Removing => write!(f, "removing"),
        }
    }
}

/// Everything about a container fghj *wants* to be true — the half of
/// `ContainerInfo` a reducer ever writes to (aside from `pending_action`),
/// and the half a convergence effect reads to decide whether/how to act on
/// Docker. Split out from `ContainerObserved` (what Docker actually
/// reports right now) because the two change on entirely different
/// triggers: `desired` only changes in response to a dispatched `Action`
/// (`RunPlanned`, `RunNodeStartRequested`, ...); `observed` only changes in
/// response to the Docker-polling effect's own inspection. Field-for-field,
/// this is the subset of `runs::ContainerInfo` that `start_node` computes
/// from a `NodeSpec` rather than reads back off a live container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContainerDesired {
    pub running: bool,
    pub container_name: String,
    pub domain: String,
    pub raw_domain: String,
    pub routes: Vec<PortRoute>,
    pub additional_hosts: Vec<String>,
    pub status_port: Option<String>,
    /// Hex-encoded hash of everything about this node's resolved config
    /// that actually affects how the container runs — see
    /// `runs::ContainerInfo::config_hash`'s doc for the exact rule and how
    /// it's compared to detect drift.
    pub config_hash: String,
}

/// Everything about a container Docker itself last reported — the
/// convergence-target-agnostic half of `ContainerInfo`; see
/// `ContainerDesired`'s doc for why the split exists. `status`/
/// `published_port`/`ports` mirror `runs::ContainerInfo`'s fields of the
/// same name one-for-one. Two fields have no 1:1 predecessor: `ip` is new
/// (the plan's `Action::ContainerObserved` carries it so raw-net/DNS
/// effects can key off a container's real network address instead of
/// re-deriving it themselves); `sync` replaces the old bare
/// `Option<bool>` (`runs::ContainerInfo::synced`) with `SyncStatus`, which
/// spells out its two different "nothing to compare" cases explicitly
/// instead of conflating them into a single `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct ContainerObserved {
    pub status: String,
    pub published_port: Option<u16>,
    pub ip: Option<String>,
    pub ports: BTreeMap<String, Option<u16>>,
    pub sync: SyncStatus,
}

/// One container's full reducer-owned state: what fghj wants
/// (`desired`), what Docker last reported (`observed`), and what's
/// currently in flight (`pending_action`). Replaces `runs::ContainerInfo`,
/// whose single flat struct mixed all three concerns together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContainerInfo {
    pub node_id: String,
    pub desired: ContainerDesired,
    pub observed: ContainerObserved,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_action: Option<PendingAction>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired() -> ContainerDesired {
        ContainerDesired {
            running: true,
            container_name: "fghj-web-1".into(),
            domain: "web.fghj.internal".into(),
            raw_domain: "web.fghj.raw.internal".into(),
            routes: vec![],
            additional_hosts: vec![],
            status_port: Some("80".into()),
            config_hash: "abc123".into(),
        }
    }

    #[test]
    fn container_info_carries_a_fresh_container_with_no_pending_action() {
        let info = ContainerInfo {
            node_id: "web".into(),
            desired: desired(),
            observed: ContainerObserved::default(),
            pending_action: None,
        };
        assert!(info.pending_action.is_none());
        assert_eq!(info.observed.sync, SyncStatus::Unknown);
    }

    #[test]
    fn pending_action_display_matches_serde_rename() {
        assert_eq!(PendingAction::Starting.to_string(), "starting");
        assert_eq!(PendingAction::Stopping.to_string(), "stopping");
        assert_eq!(PendingAction::Removing.to_string(), "removing");
    }

    #[test]
    fn pending_action_omitted_from_json_when_none() {
        let info = ContainerInfo {
            node_id: "web".into(),
            desired: desired(),
            observed: ContainerObserved::default(),
            pending_action: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("pending_action"));
    }

    #[test]
    fn pending_action_present_in_json_when_set() {
        let info = ContainerInfo {
            node_id: "web".into(),
            desired: desired(),
            observed: ContainerObserved::default(),
            pending_action: Some(PendingAction::Starting),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"pending_action\":\"starting\""));
    }
}
