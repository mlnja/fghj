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
/// About ten minutes of backoff. A node still down after this many restarts
/// won't come up by trying again (a build that fails, a config error), and
/// each try holds up every other node.
pub(crate) const MAX_ATTEMPTS: u32 = 8;

struct Tries {
    count: u32,
    last: Instant,
    given_up: bool,
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

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Heal {
    Restart {
        node_id: String,
        condition: NodeCondition,
        attempt: u32,
    },
    /// Once per node, until it's stopped or stays up after a Start by hand.
    GiveUp { node_id: String },
}

impl Healer {
    /// What to do for this workspace now. One node at a time, and only while
    /// nothing is in flight: a start brings up its requirements, and starting
    /// those in parallel would recreate them under it. The node tried longest
    /// ago goes first, so one that keeps failing can't hold up the rest.
    pub(crate) fn pick(&mut self, workspace: &str, env: &EnvState, now: Instant) -> Option<Heal> {
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
        let key = |node: &str| (workspace.to_string(), node.to_string());
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
            .filter(|(node, _)| {
                self.tries
                    .get(&key(node))
                    .is_none_or(|t| !t.given_up && t.due(now))
            })
            .min_by_key(|(node, _)| self.tries.get(&key(node)).map(|t| t.last))?;
        let tries = self.tries.entry(key(&node)).or_insert(Tries {
            count: 0,
            last: now,
            given_up: false,
        });
        if tries.count >= MAX_ATTEMPTS {
            tries.given_up = true;
            return Some(Heal::GiveUp { node_id: node });
        }
        tries.count += 1;
        tries.last = now;
        Some(Heal::Restart {
            node_id: node,
            condition,
            attempt: tries.count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PendingAction;
    use crate::state::testing::{container, run_state};

    fn restart(node_id: &str, condition: NodeCondition, attempt: u32) -> Heal {
        Heal::Restart {
            node_id: node_id.into(),
            condition,
            attempt,
        }
    }

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
            Some(restart("db", NodeCondition::Crashed, 1))
        );
        assert_eq!(healer.pick("ws", &env, t0 + Duration::from_secs(4)), None);
        assert_eq!(
            healer.pick("ws", &env, t0 + Duration::from_secs(5)),
            Some(restart("db", NodeCondition::Crashed, 2))
        );
        assert_eq!(healer.pick("ws", &env, t0 + Duration::from_secs(14)), None);
        assert_eq!(
            healer.pick("ws", &env, t0 + Duration::from_secs(15)),
            Some(restart("db", NodeCondition::Crashed, 3))
        );
    }

    #[test]
    fn the_delay_is_capped() {
        let env = run_state(vec![with_status("db", "exited")]);
        let mut healer = Healer::default();
        let mut now = Instant::now();
        for _ in 1..MAX_ATTEMPTS {
            healer.pick("ws", &env, now);
            now += MAX_DELAY;
        }
        assert!(matches!(
            healer.pick("ws", &env, now),
            Some(Heal::Restart { .. })
        ));
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
            Some(restart("db", NodeCondition::Interrupted, 1))
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
            Some(restart("db", NodeCondition::Crashed, 1))
        );
    }

    #[test]
    fn one_node_that_keeps_failing_does_not_hold_up_the_others() {
        let env = run_state(vec![
            with_status("a-build-fails", "exited"),
            with_status("db", "exited"),
        ]);
        let mut healer = Healer::default();
        let t0 = Instant::now();
        assert_eq!(
            healer.pick("ws", &env, t0),
            Some(restart("a-build-fails", NodeCondition::Crashed, 1))
        );
        // Its start took longer than its backoff: both are due again.
        let later = t0 + Duration::from_secs(120);
        assert_eq!(
            healer.pick("ws", &env, later),
            Some(restart("db", NodeCondition::Crashed, 1))
        );
        assert_eq!(
            healer.pick("ws", &env, later + Duration::from_secs(120)),
            Some(restart("a-build-fails", NodeCondition::Crashed, 2))
        );
    }

    #[test]
    fn it_gives_up_on_a_node_that_never_comes_up_until_it_is_stopped() {
        let env = run_state(vec![with_status("db", "exited")]);
        let mut healer = Healer::default();
        let mut now = Instant::now();
        for attempt in 1..=MAX_ATTEMPTS {
            assert_eq!(
                healer.pick("ws", &env, now),
                Some(restart("db", NodeCondition::Crashed, attempt))
            );
            now += MAX_DELAY;
        }
        assert_eq!(
            healer.pick("ws", &env, now),
            Some(Heal::GiveUp {
                node_id: "db".into()
            })
        );
        assert_eq!(healer.pick("ws", &env, now + MAX_DELAY), None);

        let mut stopped = with_status("db", "exited");
        stopped.desired.running = false;
        healer.pick("ws", &run_state(vec![stopped]), now);
        assert_eq!(
            healer.pick("ws", &env, now),
            Some(restart("db", NodeCondition::Crashed, 1))
        );
    }
}
