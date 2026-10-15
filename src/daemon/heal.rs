//! Brings back what crashed or was interrupted: a container fghj was asked to
//! keep up that Docker says is down. Restarts through `NodeStartRequested`, the Start button's own
//! path, so requirements come up first, in order, and the events pane narrates
//! it. Docker's restart policy does neither, which is why this isn't left to it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::state::{EnvState, NodeCondition};

const FIRST_DELAY: Duration = Duration::from_secs(5);
const MAX_DELAY: Duration = Duration::from_secs(300);
/// Up this long after a restart, a node's backoff starts over.
const STABLE: Duration = Duration::from_secs(60);

struct Tries {
    count: u32,
    last: Instant,
}

impl Tries {
    fn due(&self, now: Instant) -> bool {
        let delay = FIRST_DELAY
            .saturating_mul(1 << (self.count - 1).min(16))
            .min(MAX_DELAY);
        now.duration_since(self.last) >= delay
    }
}

/// Per (workspace, node) backoff. In memory: a restarted `fghjd` starts afresh.
#[derive(Default)]
pub(crate) struct Healer {
    tries: HashMap<(String, String), Tries>,
}

impl Healer {
    /// The node of this workspace to restart now, with its condition and
    /// attempt number. One
    /// at a time, and only while nothing is in flight: a start brings up its
    /// requirements, and starting those in parallel would recreate them under it.
    pub(crate) fn pick(
        &mut self,
        workspace: &str,
        env: &EnvState,
        now: Instant,
    ) -> Option<(String, NodeCondition, u32)> {
        if env.pending_create.is_some()
            || env.containers.values().any(|c| c.pending_action.is_some())
        {
            return None;
        }
        self.tries.retain(|(ws, node), tries| {
            ws != workspace
                || env.containers.get(node).is_some_and(|c| {
                    c.desired.running
                        && !(c.condition() == NodeCondition::Running
                            && now.duration_since(tries.last) >= STABLE)
                })
        });
        let (node, condition) = env
            .containers
            .values()
            .map(|c| (c.node_id.clone(), c.condition()))
            .filter(|(_, condition)| {
                matches!(
                    condition,
                    NodeCondition::Crashed | NodeCondition::Interrupted
                )
            })
            .find(|(node, _)| {
                self.tries
                    .get(&(workspace.to_string(), node.clone()))
                    .is_none_or(|t| t.due(now))
            })?;
        let tries = self
            .tries
            .entry((workspace.to_string(), node.clone()))
            .or_insert(Tries {
                count: 0,
                last: now,
            });
        tries.count += 1;
        tries.last = now;
        Some((node, condition, tries.count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PendingAction;
    use crate::state::testing::{container, run_state};

    fn with_status(node: &str, status: &str) -> crate::state::ContainerInfo {
        let mut c = container(node);
        c.observed.status = status.into();
        c
    }

    #[test]
    fn a_crashed_node_is_restarted_then_backs_off() {
        let env = run_state(vec![with_status("db", "exited")]);
        let mut healer = Healer::default();
        let t0 = Instant::now();
        assert_eq!(
            healer.pick("ws", &env, t0),
            Some(("db".into(), NodeCondition::Crashed, 1))
        );
        assert_eq!(healer.pick("ws", &env, t0 + Duration::from_secs(4)), None);
        assert_eq!(
            healer.pick("ws", &env, t0 + Duration::from_secs(5)),
            Some(("db".into(), NodeCondition::Crashed, 2))
        );
        assert_eq!(healer.pick("ws", &env, t0 + Duration::from_secs(14)), None);
        assert_eq!(
            healer.pick("ws", &env, t0 + Duration::from_secs(15)),
            Some(("db".into(), NodeCondition::Crashed, 3))
        );
    }

    #[test]
    fn the_delay_is_capped() {
        let env = run_state(vec![with_status("db", "exited")]);
        let mut healer = Healer::default();
        let mut now = Instant::now();
        for _ in 0..20 {
            healer.pick("ws", &env, now);
            now += MAX_DELAY;
        }
        assert!(healer.pick("ws", &env, now).is_some());
    }

    #[test]
    fn stopped_missing_running_and_tasks_are_left_alone() {
        let mut stopped = with_status("stopped", "exited");
        stopped.desired.running = false;
        let mut task = with_status("migrate", "exited");
        task.desired.terminating = true;
        task.observed.exit_code = Some(1);
        let env = run_state(vec![
            stopped,
            task,
            with_status("gone", "removed"),
            with_status("up", "running"),
            with_status("halted", "paused"),
        ]);
        assert_eq!(Healer::default().pick("ws", &env, Instant::now()), None);
    }

    #[test]
    fn an_interrupted_node_is_brought_back_but_an_unknown_one_waits() {
        let mut down = with_status("db", "exited");
        down.observed.interrupted = true;
        assert_eq!(
            Healer::default().pick("ws", &run_state(vec![down]), Instant::now()),
            Some(("db".into(), NodeCondition::Interrupted, 1))
        );
        let mut unseen = with_status("db", "exited");
        unseen.observed.unseen = true;
        let blind = run_state(vec![unseen]);
        assert_eq!(Healer::default().pick("ws", &blind, Instant::now()), None);
    }

    #[test]
    fn nothing_is_restarted_while_anything_is_in_flight() {
        let mut busy = with_status("api", "running");
        busy.pending_action = Some(PendingAction::Starting);
        let env = run_state(vec![busy, with_status("db", "exited")]);
        assert_eq!(Healer::default().pick("ws", &env, Instant::now()), None);
    }

    #[test]
    fn staying_up_resets_the_backoff() {
        let mut healer = Healer::default();
        let t0 = Instant::now();
        healer.pick("ws", &run_state(vec![with_status("db", "exited")]), t0);
        let up = run_state(vec![with_status("db", "running")]);
        healer.pick("ws", &up, t0 + STABLE);
        let crashed = run_state(vec![with_status("db", "exited")]);
        assert_eq!(
            healer.pick("ws", &crashed, t0 + STABLE),
            Some(("db".into(), NodeCondition::Crashed, 1))
        );
    }
}
