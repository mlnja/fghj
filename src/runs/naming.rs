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
/// volume's `scope` is the same knob as `domain_scope`).
///
/// `owner` is the id of the node that declared the volume, and passing it is
/// what makes the label private to that node: `data` declared by
/// `db.cart.shop` and `data` declared by `db.orders.warehouse` are two
/// volumes, the same way two services both named `api` are two nodes. This
/// is the leaf-first qualification the rest of the system applies
/// everywhere, finally applied here too.
///
/// `None` means the author set `#Volume.shared`, dropping the
/// qualification so the label alone decides identity. That is a real feature
/// — but the default has to be the other way around. An unqualified volume
/// namespace lets two repos that each declare `{name: data, scope: stable}`
/// on their own Postgres end up with one volume and two engines writing to
/// it: silent corruption, from two individually valid configs, written by
/// teams who have never spoken. See `concepts/AUDIT.md` B3.
pub(crate) fn derive_volume_name(
    name: &str,
    scope: &str,
    owner: Option<&str>,
    workspace_name: &str,
    run_id: &str,
) -> String {
    // Qualifying by appending the owner id mirrors how a backing node's own
    // id is built (`{dep.name}.{owner_id}`), so the derived volume name
    // reads as a path down the same tree rather than as a mangled label.
    let key = match owner {
        Some(owner) => format!("{name}.{owner}"),
        None => name.to_string(),
    };
    format!(
        "fghj-vol-{}",
        sanitize_label(&derive_domain(
            &key,
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
    fn scope_and_run_id_fold_in_the_same_way_they_do_for_a_domain() {
        let a = derive_volume_name("cache", "run", Some("web.shop"), "shop", "preview-1");
        let b = derive_volume_name("cache", "run", Some("web.shop"), "shop", "preview-1");
        assert_eq!(a, b);

        // "stable" never folds in the run id, so it must differ from a
        // "run"-scoped name for the same non-default run.
        let stable = derive_volume_name("cache", "stable", Some("web.shop"), "shop", "preview-1");
        assert_ne!(a, stable);

        // A different named run gets its own fresh "run"-scoped volume.
        let other_run = derive_volume_name("cache", "run", Some("web.shop"), "shop", "preview-2");
        assert_ne!(a, other_run);
    }

    /// B3: the same label declared by two unrelated nodes used to be one
    /// Docker volume — two engines, one data directory, no warning.
    #[test]
    fn the_same_label_on_two_nodes_is_two_volumes_by_default() {
        let a = derive_volume_name("data", "stable", Some("db.cart.shop"), "ws", "default");
        let b = derive_volume_name(
            "data",
            "stable",
            Some("db.orders.warehouse"),
            "ws",
            "default",
        );
        assert_ne!(a, b);
    }

    /// ...and opting in still gets you exactly one.
    #[test]
    fn shared_volumes_ignore_the_owner_and_collapse_onto_one_name() {
        let a = derive_volume_name("data", "stable", None, "ws", "default");
        let b = derive_volume_name("data", "stable", None, "ws", "default");
        assert_eq!(a, b);
        // And a shared volume is never the same name as a private one,
        // so opting in mid-life is a visible migration, not a silent
        // adoption of somebody else's data.
        let private = derive_volume_name("data", "stable", Some("db.cart.shop"), "ws", "default");
        assert_ne!(a, private);
    }
}
