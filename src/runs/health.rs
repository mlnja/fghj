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
}
