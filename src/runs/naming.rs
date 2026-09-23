use super::domain::{DomainZone, derive_domain};
use crate::util::label::sanitize_label;

pub const DEFAULT_RUN_ID: &str = "default";

/// The concrete run id `start` derives from a `RunSpec::run_id` — sanitized,
/// falling back to `DEFAULT_RUN_ID` when absent or empty after sanitizing.
/// Exposed so `daemon::post_runs` can compute the same id up front to
/// dispatch `Action::RunPlanned` under, before `start`/`ensure_running`
/// (now called from inside `effects::docker::converge::perform_create`,
/// not synchronously from the HTTP handler) ever runs.
pub fn resolve_run_id(run_id: Option<&str>) -> String {
    run_id
        .map(sanitize_label)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_RUN_ID.to_string())
}

/// Derives the real Docker volume name for a `VolumeMount::Named` entry —
/// reuses `derive_domain`'s exact run/stable folding logic (a named
/// volume's `scope` is the same knob as `domain_scope`), keyed by the
/// volume's own declared `name` instead of a node id. Two nodes anywhere in
/// the graph that declare the same `name` + `scope` therefore land on the
/// same derived value here and transparently share one Docker volume.
pub(crate) fn derive_volume_name(
    name: &str,
    scope: &str,
    workspace_name: &str,
    run_id: &str,
) -> String {
    format!(
        "fghj-vol-{}",
        sanitize_label(&derive_domain(
            name,
            scope,
            workspace_name,
            run_id,
            DomainZone::Http,
        ))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_run_id_falls_back_to_default_when_absent_or_empty() {
        assert_eq!(resolve_run_id(None), DEFAULT_RUN_ID);
        assert_eq!(resolve_run_id(Some("")), DEFAULT_RUN_ID);
        assert_eq!(resolve_run_id(Some("///")), DEFAULT_RUN_ID);
    }

    #[test]
    fn resolve_run_id_sanitizes_a_given_id() {
        assert_eq!(resolve_run_id(Some("Feature/JIRA-123")), "feature-jira-123");
    }

    #[test]
    fn two_nodes_sharing_a_named_volume_derive_the_same_docker_name() {
        // Keyed by the declared `name`, not any node id — two unrelated
        // nodes (service or backing) that declare the same `name` + `scope`
        // land on the same derived value and therefore the same Docker volume.
        let a = derive_volume_name("cache", "run", "shop", "preview-1");
        let b = derive_volume_name("cache", "run", "shop", "preview-1");
        assert_eq!(a, b);

        // "stable" never folds in the run id, so it must differ from a
        // "run"-scoped name for the same non-default run.
        let stable = derive_volume_name("cache", "stable", "shop", "preview-1");
        assert_ne!(a, stable);

        // A different named run gets its own fresh "run"-scoped volume.
        let other_run = derive_volume_name("cache", "run", "shop", "preview-2");
        assert_ne!(a, other_run);
    }
}
