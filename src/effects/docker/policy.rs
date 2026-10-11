//! Container drift policy. Only `ObserverOnly` is ever constructed.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DockerHealPolicy {
    #[default]
    ObserverOnly,
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
