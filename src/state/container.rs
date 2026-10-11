use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::sync_status::SyncStatus;

/// A domain this container answers to and the host port behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRoute {
    pub domain: String,
    /// As of route derivation. Read through `state::query::live_host_port`,
    /// which prefers the freshly observed binding.
    pub host_port: u16,
    /// `domain` is a `wildcard_hosts` suffix: matches it and its subdomains.
    pub wildcard: bool,
    pub container_port: String,
    /// Whether fghj's CA can certify `domain`; false only for an alias on a real
    /// TLD, which the UI links as `http://`.
    pub https: bool,
}

/// The start/stop/delete in flight. The only busy gate: the reducer sets it,
/// rejects requests while it's set, and clears it on settle.
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

/// The checkout a container was built from, as plain text so drift can name a
/// commit. A snapshot taken at start, never refreshed. `None` for nodes fghj
/// doesn't build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ContainerSource {
    pub branch: Option<String>,
    /// `None` when git couldn't be read.
    pub head: Option<String>,
    /// One bit: catches the first clean → dirty edit, nothing after.
    pub dirty: bool,
}

/// What fghj wants. Computed from a `NodeSpec`, never read off a live
/// container, so `observed != desired` means something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct ContainerDesired {
    /// Set from the intent of the call, never from Docker's status.
    pub running: bool,
    pub container_name: String,
    pub domain: String,
    pub raw_domain: String,
    pub routes: Vec<PortRoute>,
    /// The `additional_hosts` that got a route — what `/etc/hosts` needs.
    pub additional_hosts: Vec<String>,
    /// The port `status`/`published_port` are inspected against.
    pub status_port: Option<String>,
    /// `orchestrator::spec::spec_hash` at the last start; compared for drift.
    pub config_hash: String,
    pub source: Option<ContainerSource>,
    /// A task (`kind: "task"`): "exited 0" is its goal, so exited means success,
    /// not drift. `running` stays false for a task.
    pub terminating: bool,
    /// Started with `FGHJ_DEBUG_WAIT=1`. Runtime state flipped from the UI, not
    /// config, and not in `spec_hash`, so a drift recreate drops it — which the
    /// UI then shows truthfully.
    pub debug_wait: bool,
}

/// What Docker last reported. Only written from a real inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct ContainerObserved {
    pub status: String,
    pub published_port: Option<u16>,
    pub ip: Option<String>,
    /// Every declared port's host binding; `None` if not published.
    pub ports: BTreeMap<String, Option<u16>>,
    pub sync: SyncStatus,
    /// `None` while running. A task's outcome; for a service just a detail.
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    pub node_id: String,
    pub desired: ContainerDesired,
    pub observed: ContainerObserved,
    pub pending_action: Option<PendingAction>,
}

/// What a container's `desired`/`observed` pair means, in one word. Derived,
/// never stored. `Crashed` is the one that matters: nothing converges it, so
/// the user has to be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeCondition {
    Running,
    Stopped,
    Crashed,
    Restarting,
    /// Not serving; a Start won't help, it isn't stopped.
    Paused,
    Completed,
    Failed,
    /// Exited, but not re-inspected yet: no exit code, no verdict.
    Finishing,
}

impl ContainerInfo {
    /// Ignores `pending_action`; consumers layer that on top.
    pub fn condition(&self) -> NodeCondition {
        let status = self.observed.status.as_str();
        if self.desired.terminating {
            return match (status, self.observed.exit_code) {
                ("running", _) => NodeCondition::Running,
                (_, Some(0)) => NodeCondition::Completed,
                (_, Some(_)) => NodeCondition::Failed,
                (_, None) => NodeCondition::Finishing,
            };
        }
        match status {
            "running" => NodeCondition::Running,
            "restarting" => NodeCondition::Restarting,
            "paused" => NodeCondition::Paused,
            _ if self.desired.running => NodeCondition::Crashed,
            _ => NodeCondition::Stopped,
        }
    }
}

/// Hand-written to add the derived `condition`, which must never be stored.
impl Serialize for ContainerInfo {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let fields = 4 + usize::from(self.pending_action.is_some());
        let mut s = serializer.serialize_struct("ContainerInfo", fields)?;
        s.serialize_field("node_id", &self.node_id)?;
        s.serialize_field("desired", &self.desired)?;
        s.serialize_field("observed", &self.observed)?;
        s.serialize_field("condition", &self.condition())?;
        if let Some(pending) = &self.pending_action {
            s.serialize_field("pending_action", pending)?;
        }
        s.end()
    }
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
            source: None,
            terminating: false,
            debug_wait: false,
        }
    }

    fn info(status: &str, running: bool) -> ContainerInfo {
        ContainerInfo {
            node_id: "web".into(),
            desired: ContainerDesired {
                running,
                ..desired()
            },
            observed: ContainerObserved {
                status: status.into(),
                ..Default::default()
            },
            pending_action: None,
        }
    }

    fn task(status: &str, exit_code: Option<i64>) -> ContainerInfo {
        ContainerInfo {
            node_id: "migrate".into(),
            desired: ContainerDesired {
                running: false,
                terminating: true,
                ..desired()
            },
            observed: ContainerObserved {
                status: status.into(),
                exit_code,
                ..Default::default()
            },
            pending_action: None,
        }
    }

    #[test]
    fn asked_to_be_up_and_observed_down_is_crashed_not_stopped() {
        assert_eq!(info("exited", true).condition(), NodeCondition::Crashed);
        assert_eq!(info("exited", false).condition(), NodeCondition::Stopped);
    }

    #[test]
    fn every_not_up_status_reads_the_same_way() {
        for status in ["exited", "dead", "removed", "created"] {
            assert_eq!(
                info(status, true).condition(),
                NodeCondition::Crashed,
                "{status} while desired-running"
            );
            assert_eq!(
                info(status, false).condition(),
                NodeCondition::Stopped,
                "{status} while not desired-running"
            );
        }
    }

    #[test]
    fn restarting_and_paused_are_neither_running_nor_stopped() {
        assert_eq!(
            info("restarting", true).condition(),
            NodeCondition::Restarting
        );
        assert_eq!(info("paused", true).condition(), NodeCondition::Paused);
    }

    #[test]
    fn a_task_is_read_on_its_exit_code_not_on_being_down() {
        assert_eq!(
            task("exited", Some(0)).condition(),
            NodeCondition::Completed
        );
        assert_eq!(task("exited", Some(1)).condition(), NodeCondition::Failed);
        assert_eq!(task("exited", None).condition(), NodeCondition::Finishing);
        assert_eq!(task("running", None).condition(), NodeCondition::Running);
    }

    #[test]
    fn condition_is_serialized_alongside_the_pair_it_reads() {
        let json = serde_json::to_value(info("exited", true)).unwrap();
        assert_eq!(json["condition"], "crashed");
        assert_eq!(json["observed"]["status"], "exited");
        assert_eq!(json["desired"]["running"], true);
        assert!(json.get("pending_action").is_none());
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
