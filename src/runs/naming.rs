use super::domain::{DomainZone, derive_domain};
use crate::util::label::sanitize_label;

/// The key the workspace's one environment is filed under — in state, in
/// the database, and in its network and container names.
pub const DEFAULT_RUN_ID: &str = "default";

/// The environment's Docker network. Every name below embeds `run_id`, and
/// existing containers are found by these exact names, so their shape is
/// a compatibility contract — see `names_keep_their_shape`.
pub(crate) fn network_name(workspace_name: &str, run_id: &str) -> String {
    format!("fghj-{}-{run_id}", sanitize_label(workspace_name))
}

/// The container a node runs in.
pub(crate) fn container_name(workspace_name: &str, run_id: &str, node_id: &str) -> String {
    format!(
        "fghj-{}-{run_id}-{}",
        sanitize_label(workspace_name),
        sanitize_label(node_id)
    )
}

/// The environment's in-network TLS proxy sidecar.
pub(crate) fn sidecar_name(workspace_name: &str, run_id: &str) -> String {
    format!("fghj-{}-{run_id}-sidecar", sanitize_label(workspace_name))
}

/// Derives the real Docker volume name for a `VolumeMount::Named` entry —
/// the same shape as a node's domain, so the two read alike.
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
/// namespace lets two repos that each declare `{name: data}`
/// on their own Postgres end up with one volume and two engines writing to
/// it: silent corruption, from two individually valid configs, written by
/// teams who have never spoken. See `concepts/AUDIT.md` B3.
pub(crate) fn derive_volume_name(name: &str, owner: Option<&str>, workspace_name: &str) -> String {
    // Qualifying by appending the owner id mirrors how a backing node's own
    // id is built (`{dep.name}.{owner_id}`), so the derived volume name
    // reads as a path down the same tree rather than as a mangled label.
    let key = match owner {
        Some(owner) => format!("{name}.{owner}"),
        None => name.to_string(),
    };
    format!(
        "fghj-vol-{}",
        sanitize_label(&derive_domain(&key, workspace_name, DomainZone::Http))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_keep_their_shape() {
        assert_eq!(network_name("Shop", "default"), "fghj-shop-default");
        assert_eq!(
            container_name("Shop", "default", "api.orders"),
            "fghj-shop-default-api-orders"
        );
        assert_eq!(sidecar_name("Shop", "default"), "fghj-shop-default-sidecar");
    }

    /// B3: the same label declared by two unrelated nodes used to be one
    /// Docker volume — two engines, one data directory, no warning.
    #[test]
    fn the_same_label_on_two_nodes_is_two_volumes_by_default() {
        let a = derive_volume_name("data", Some("db.cart.shop"), "ws");
        let b = derive_volume_name("data", Some("db.orders.warehouse"), "ws");
        assert_ne!(a, b);
    }

    /// ...and opting in still gets you exactly one.
    #[test]
    fn shared_volumes_ignore_the_owner_and_collapse_onto_one_name() {
        let a = derive_volume_name("data", None, "ws");
        let b = derive_volume_name("data", None, "ws");
        assert_eq!(a, b);
        // And a shared volume is never the same name as a private one,
        // so opting in mid-life is a visible migration, not a silent
        // adoption of somebody else's data.
        let private = derive_volume_name("data", Some("db.cart.shop"), "ws");
        assert_ne!(a, private);
    }
}
