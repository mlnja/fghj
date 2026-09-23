//! The switch named in the plan's "Container drift policy: observer-only
//! by default" section: flipping `effects::docker::converge::DockerConvergeEffect::extract`
//! to fold `observed` into its snapshot (alongside `desired`) would make it
//! try to restart a container `effects::docker::observe` reports as
//! crashed, rather than just surfacing the drift. That's a real behavior
//! change with its own tradeoffs (a flapping container now gets retried
//! instead of staying down for a human to look at), so it's deliberately
//! gated behind this enum rather than done implicitly — `ObserverOnly` is
//! the only variant anything constructs today; `AutoHeal` exists so that
//! future change is a one-function `extract` edit, not a new
//! `Action`/reducer/state-shape change.

/// How `effects::docker::converge` should treat a container `converge`
/// itself didn't put into its current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DockerHealPolicy {
    /// Report drift (`observed != desired`) for the UI to show; never act
    /// on it. The only policy anything runs today.
    #[default]
    ObserverOnly,
    /// Also treat observed drift as something `converge` should correct —
    /// unused until something actually constructs this variant.
    AutoHeal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_observer_only() {
        assert_eq!(DockerHealPolicy::default(), DockerHealPolicy::ObserverOnly);
    }
}
