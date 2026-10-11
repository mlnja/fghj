use std::time::{Duration, Instant};

use crate::docker;

pub(crate) const PER_NODE_LIMIT: Duration = Duration::from_secs(120);

/// Bounds a whole run: nodes start sequentially, so the per-node limit alone
/// doesn't bound anything a person waits on.
pub(crate) const RUN_LIMIT: Duration = Duration::from_secs(300);

/// The remaining health-wait allowance for one whole-run operation. Running out
/// truncates the wait, not the run.
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

    /// Whatever is left, capped at the per-node limit.
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

/// Separate deadlines: a spent health budget means stop waiting, a spent task
/// budget fails the task. Sharing one would let a slow healthcheck fail an
/// unrelated migration.
#[derive(Debug, Clone, Default)]
pub(crate) struct RunBudget {
    pub(crate) health: HealthBudget,
    pub(crate) tasks: HealthBudget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HealthOutcome {
    Healthy,
    /// `unhealthy`, vanished, or no healthcheck: nothing left to wait for.
    Settled,
    TimedOut,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskOutcome {
    Completed,
    /// Exited non-zero, or vanished before reporting a code.
    Failed {
        exit_code: Option<i64>,
    },
    TimedOut,
}

pub(crate) async fn wait_for_exit(
    docker: &bollard::Docker,
    container_name: &str,
    allowance: Duration,
) -> TaskOutcome {
    const POLL: Duration = Duration::from_millis(500);
    let deadline = Instant::now() + allowance;
    loop {
        match docker::inspect_status(docker, container_name, "").await {
            Ok(None) => return TaskOutcome::Failed { exit_code: None },
            Ok(Some(status)) if status.status != "running" && status.status != "created" => {
                return match status.exit_code {
                    Some(0) => TaskOutcome::Completed,
                    code => TaskOutcome::Failed { exit_code: code },
                };
            }
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

    fn about(actual: Duration, expected: Duration) -> bool {
        expected.saturating_sub(actual) < Duration::from_secs(1)
    }

    #[test]
    fn a_fresh_run_budget_allows_a_full_per_node_wait() {
        let budget = HealthBudget::default();
        assert!(about(budget.allowance(), PER_NODE_LIMIT), "{budget:?}");
        assert!(!budget.is_exhausted());
    }

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

    #[test]
    fn a_partly_spent_budget_hands_out_only_the_remainder() {
        let budget = HealthBudget::new(Duration::from_secs(5));
        let allowance = budget.allowance();
        assert!(allowance <= Duration::from_secs(5), "{allowance:?}");
        assert!(!allowance.is_zero());
    }

    #[test]
    fn the_run_limit_bounds_the_total_not_each_node() {
        assert!(RUN_LIMIT < PER_NODE_LIMIT * 12);
    }

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

    /// Not `--rm`: the exit code would go with the container.
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

    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_task_that_exits_nonzero_is_failed_with_its_code() {
        let c = ExitingContainer::start(&["sh", "-c", "exit 7"]);
        let outcome = wait_for_exit(&docker_client(), &c.name, PER_NODE_LIMIT).await;
        assert_eq!(outcome, TaskOutcome::Failed { exit_code: Some(7) });
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_task_that_never_finishes_times_out() {
        let c = ExitingContainer::start(&["sleep", "60"]);
        let outcome = wait_for_exit(&docker_client(), &c.name, Duration::from_secs(2)).await;
        assert_eq!(outcome, TaskOutcome::TimedOut);
    }

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
