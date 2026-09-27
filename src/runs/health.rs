//! Waiting for a container's declared healthcheck, and the budget that
//! keeps a whole run from spending all day doing it.

use std::time::{Duration, Instant};

use crate::docker;

/// How long one node's healthcheck may take. Long enough for a real
/// database's own startup check, short enough that a genuinely broken one
/// doesn't hang a run forever.
pub(crate) const PER_NODE_LIMIT: Duration = Duration::from_secs(120);

/// How long an entire run may spend waiting on healthchecks, in total.
///
/// The per-node limit alone was not a bound on anything that matters: nodes
/// are started sequentially, so an N-node run worst-cased at N × 120 s with
/// no ceiling. Twelve slow nodes meant a twenty-four-minute "start" with no
/// indication anything was wrong.
///
/// A shared budget makes the whole run the unit that is bounded, which is
/// the unit a person is actually waiting on.
pub(crate) const RUN_LIMIT: Duration = Duration::from_secs(300);

/// The remaining health-wait allowance for one whole-run operation.
///
/// Truncating the *health wait* rather than the run is deliberate, and it
/// is the same call `wait_for_healthy` already made at its own limit:
/// waiting is best-effort, so when it runs out fghj proceeds instead of
/// failing. Containers still get created; later nodes simply stop waiting
/// for health first. Refusing to start the rest of an environment because a
/// clock ran out would turn a slow start into no environment at all, which
/// is strictly worse.
#[derive(Debug, Clone)]
pub(crate) struct HealthBudget {
    deadline: Instant,
}

impl HealthBudget {
    pub(crate) fn new(total: Duration) -> Self {
        HealthBudget {
            deadline: Instant::now() + total,
        }
    }

    /// A budget for a single-node operation: nothing else is sharing it, so
    /// the per-node limit is the whole of it.
    pub(crate) fn single_node() -> Self {
        HealthBudget::new(PER_NODE_LIMIT)
    }

    /// What this node may spend: whatever is left, capped at the per-node
    /// limit so one slow node cannot consume the entire run's allowance.
    pub(crate) fn allowance(&self) -> Duration {
        self.deadline
            .saturating_duration_since(Instant::now())
            .min(PER_NODE_LIMIT)
    }

    pub(crate) fn is_exhausted(&self) -> bool {
        self.allowance().is_zero()
    }
}

impl Default for HealthBudget {
    fn default() -> Self {
        HealthBudget::new(RUN_LIMIT)
    }
}

/// The two independent deadlines one whole-run operation spends waiting.
///
/// Kept apart rather than shared because they answer differently when they
/// run out. A spent *health* budget means fghj stops waiting and carries on
/// — the container is up either way, and refusing to start the rest of an
/// environment over a slow healthcheck would be strictly worse than a
/// truncated wait. A spent *task* budget fails the task, and a failed task
/// blocks every node downstream of it, because the whole reason the
/// terminating kind exists is that dependents must not start until it has
/// actually finished.
///
/// One shared deadline would let a slow postgres healthcheck earlier in the
/// run fail an unrelated migration that had not even started waiting yet —
/// a best-effort wait silently escalating into a hard failure somewhere
/// else.
#[derive(Debug, Clone, Default)]
pub(crate) struct RunBudget {
    pub(crate) health: HealthBudget,
    pub(crate) tasks: HealthBudget,
}

impl RunBudget {
    /// A budget for a single-node operation: nothing else is sharing either
    /// half of it.
    pub(crate) fn single_node() -> Self {
        RunBudget {
            health: HealthBudget::single_node(),
            tasks: HealthBudget::single_node(),
        }
    }
}

/// Why a health wait stopped — recorded into the node's event stream so a
/// truncated wait is visible rather than looking like a clean pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HealthOutcome {
    /// The container reported `healthy`.
    Healthy,
    /// The container reported `unhealthy`, or vanished, or declares no
    /// healthcheck — either way there is nothing left to wait for.
    Settled,
    /// The allowance ran out first. The container is still running; fghj
    /// simply stopped waiting.
    TimedOut,
}

/// Polls a container's declared healthcheck until it reports `healthy`, for
/// up to `allowance`. Returns as soon as there is nothing more to wait for:
/// no declared healthcheck, a terminal `unhealthy` report (best-effort —
/// fghj proceeds rather than blocking the run indefinitely), or the
/// container having vanished. Callers only call this at all when
/// `node.healthcheck.is_some()`, but it is written to be a safe no-op
/// otherwise too.
pub(crate) async fn wait_for_healthy(
    docker: &bollard::Docker,
    container_name: &str,
    allowance: Duration,
) -> HealthOutcome {
    const POLL: Duration = Duration::from_secs(2);
    let deadline = Instant::now() + allowance;
    loop {
        match docker::inspect_health(docker, container_name).await {
            Ok(Some(status)) if status == "healthy" => return HealthOutcome::Healthy,
            Ok(Some(status)) if status == "unhealthy" => return HealthOutcome::Settled,
            Ok(None) => return HealthOutcome::Settled,
            _ => {}
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return HealthOutcome::TimedOut;
        }
        tokio::time::sleep(POLL.min(left)).await;
    }
}

/// How a terminating node's (`Node.kind == "task"`) container finished —
/// the task-shaped counterpart of `HealthOutcome`. The distinction
/// `HealthOutcome` deliberately does not draw, between "settled" and
/// "settled *well*", is the entire content of this one: a migration that
/// exited 3 and a migration that exited 0 are both "no longer running".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskOutcome {
    /// Exited 0. The only outcome that lets dependents start.
    Completed,
    /// Exited non-zero, or vanished before Docker could report a code
    /// (`exit_code: None`) — something removed the container mid-wait, which
    /// is no more evidence the task succeeded than a non-zero code is.
    Failed { exit_code: Option<i64> },
    /// Still running when the allowance ran out. Unlike a truncated health
    /// wait this is a failure, not a shrug: fghj never saw this task finish,
    /// so it cannot honestly let anything that depends on it start.
    TimedOut,
}

/// Polls a container until it stops running, for up to `allowance`, and
/// reports how it finished.
///
/// The terminating-node counterpart to `wait_for_healthy`, and deliberately
/// not a variation on it: that one waits for a container to become *ready*
/// and treats running-forever as the success case, this one waits for a
/// container to be *done* and treats running-forever as the failure case.
pub(crate) async fn wait_for_exit(
    docker: &bollard::Docker,
    container_name: &str,
    allowance: Duration,
) -> TaskOutcome {
    const POLL: Duration = Duration::from_millis(500);
    let deadline = Instant::now() + allowance;
    loop {
        match docker::inspect_status(docker, container_name, "").await {
            // Gone. Nothing left to wait for and nothing that proves it
            // ran, so this is a failure with no code to report.
            Ok(None) => return TaskOutcome::Failed { exit_code: None },
            Ok(Some(status)) if status.status != "running" && status.status != "created" => {
                return match status.exit_code {
                    Some(0) => TaskOutcome::Completed,
                    code => TaskOutcome::Failed { exit_code: code },
                };
            }
            // Still running, or a transient inspect error — either way
            // there is nothing to conclude yet.
            _ => {}
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return TaskOutcome::TimedOut;
        }
        tokio::time::sleep(POLL.min(left)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Allowances are computed against the real clock, so they come back a
    /// few microseconds shy of the nominal figure. Compare with a tolerance
    /// wide enough to absorb scheduling noise and far too narrow to hide an
    /// actual budgeting mistake.
    fn about(actual: Duration, expected: Duration) -> bool {
        expected.saturating_sub(actual) < Duration::from_secs(1)
    }

    #[test]
    fn a_fresh_run_budget_allows_a_full_per_node_wait() {
        let budget = HealthBudget::default();
        assert!(about(budget.allowance(), PER_NODE_LIMIT), "{budget:?}");
        assert!(!budget.is_exhausted());
    }

    /// One slow node must not be able to eat the whole run's allowance —
    /// otherwise the budget would just relocate the problem to whichever
    /// node happened to be first.
    #[test]
    fn no_single_node_may_spend_more_than_the_per_node_limit() {
        let generous = HealthBudget::new(PER_NODE_LIMIT * 10);
        assert!(generous.allowance() <= PER_NODE_LIMIT);
        assert!(about(generous.allowance(), PER_NODE_LIMIT));
    }

    #[test]
    fn an_exhausted_budget_allows_nothing_and_says_so() {
        let spent = HealthBudget::new(Duration::ZERO);
        assert!(spent.is_exhausted());
        assert_eq!(spent.allowance(), Duration::ZERO);
    }

    /// A budget smaller than the per-node limit hands out what is left, not
    /// the per-node limit — that is the whole mechanism.
    #[test]
    fn a_partly_spent_budget_hands_out_only_the_remainder() {
        let budget = HealthBudget::new(Duration::from_secs(5));
        let allowance = budget.allowance();
        assert!(allowance <= Duration::from_secs(5), "{allowance:?}");
        assert!(!allowance.is_zero());
    }

    /// A single-node operation shares its budget with nothing, so it gets
    /// the full per-node limit regardless of what any run is doing.
    #[test]
    fn a_single_node_operation_gets_the_full_per_node_limit() {
        let budget = HealthBudget::single_node();
        assert!(budget.allowance() <= PER_NODE_LIMIT);
        assert!(about(budget.allowance(), PER_NODE_LIMIT), "{budget:?}");
    }

    /// The bound that matters: a whole run can't exceed the run limit no
    /// matter how many nodes it has.
    #[test]
    fn the_run_limit_bounds_the_total_not_each_node() {
        assert!(RUN_LIMIT < PER_NODE_LIMIT * 12);
    }

    /// A run's two deadlines must not be the same one: `RunBudget` exists
    /// precisely so a slow healthcheck early in a run cannot spend the
    /// allowance a later migration needs.
    #[test]
    fn a_run_budget_hands_health_and_tasks_independent_deadlines() {
        let budget = RunBudget::default();
        assert!(!budget.health.is_exhausted());
        assert!(!budget.tasks.is_exhausted());
        let spent = RunBudget {
            health: HealthBudget::new(Duration::ZERO),
            tasks: HealthBudget::default(),
        };
        assert!(spent.health.is_exhausted());
        assert!(!spent.tasks.is_exhausted());
    }

    /// A real, un-`--rm`'d container running `cmd`, torn down on drop.
    /// Not `--rm`: the whole point of these tests is the exit code, and a
    /// self-removing container takes it with it.
    struct ExitingContainer {
        name: String,
    }

    impl ExitingContainer {
        fn start(cmd: &[&str]) -> Self {
            let name = format!(
                "fghj-wait-for-exit-test-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let mut args = vec!["run", "-d", "--name", name.as_str(), "busybox"];
            args.extend_from_slice(cmd);
            let status = std::process::Command::new("docker")
                .args(&args)
                .status()
                .expect("failed to run `docker run` for wait_for_exit fixture");
            assert!(
                status.success(),
                "docker run failed for wait_for_exit fixture"
            );
            Self { name }
        }
    }

    impl Drop for ExitingContainer {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &self.name])
                .status();
        }
    }

    fn docker_client() -> bollard::Docker {
        crate::daemon::connect_docker().expect("docker client for test")
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_task_that_exits_zero_is_completed() {
        let c = ExitingContainer::start(&["true"]);
        let outcome = wait_for_exit(&docker_client(), &c.name, PER_NODE_LIMIT).await;
        assert_eq!(outcome, TaskOutcome::Completed);
    }

    /// The code itself has to survive, not just the fact of failure: it is
    /// what a person reads to find out *why* their migration blocked the
    /// service behind it.
    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_task_that_exits_nonzero_is_failed_with_its_code() {
        let c = ExitingContainer::start(&["sh", "-c", "exit 7"]);
        let outcome = wait_for_exit(&docker_client(), &c.name, PER_NODE_LIMIT).await;
        assert_eq!(outcome, TaskOutcome::Failed { exit_code: Some(7) });
    }

    /// The case that separates this from `wait_for_healthy`: a container
    /// that just keeps running is the *success* condition there and the
    /// failure condition here.
    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_task_that_never_finishes_times_out() {
        let c = ExitingContainer::start(&["sleep", "60"]);
        let outcome = wait_for_exit(&docker_client(), &c.name, Duration::from_secs(2)).await;
        assert_eq!(outcome, TaskOutcome::TimedOut);
    }

    /// A container that vanished mid-wait proves nothing about whether its
    /// command succeeded, so it must not read as success — that would let
    /// `docker rm -f` on a migration silently unblock the service waiting
    /// on it.
    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_task_container_that_is_gone_is_failed_not_completed() {
        let outcome = wait_for_exit(
            &docker_client(),
            "fghj-wait-for-exit-test-does-not-exist",
            PER_NODE_LIMIT,
        )
        .await;
        assert_eq!(outcome, TaskOutcome::Failed { exit_code: None });
    }
}
