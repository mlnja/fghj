use super::domain::{DomainZone, derive_domain};
use crate::util::label::sanitize_label;

// The `-default-` is a leftover of named runs. Existing containers are found by
// these exact names, so don't change them — see `names_keep_their_shape`.

pub(crate) fn network_name(workspace_name: &str) -> String {
    format!("fghj-{}-default", sanitize_label(workspace_name))
}

pub(crate) fn container_name(workspace_name: &str, node_id: &str) -> String {
    format!(
        "fghj-{}-default-{}",
        sanitize_label(workspace_name),
        sanitize_label(node_id)
    )
}

pub(crate) fn sidecar_name(workspace_name: &str) -> String {
    format!("fghj-{}-default-sidecar", sanitize_label(workspace_name))
}

/// The Docker volume for a named mount. Qualified by the declaring node's id
/// unless `#Volume.shared`, so two repos' `data` volumes never collide.
pub(crate) fn derive_volume_name(name: &str, owner: Option<&str>, workspace_name: &str) -> String {
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
        assert_eq!(network_name("Shop"), "fghj-shop-default");
        assert_eq!(
            container_name("Shop", "api.orders"),
            "fghj-shop-default-api-orders"
        );
        assert_eq!(sidecar_name("Shop"), "fghj-shop-default-sidecar");
    }

    #[test]
    fn the_same_label_on_two_nodes_is_two_volumes_by_default() {
        let a = derive_volume_name("data", Some("db.cart.shop"), "ws");
        let b = derive_volume_name("data", Some("db.orders.warehouse"), "ws");
        assert_ne!(a, b);
    }

    #[test]
    fn shared_volumes_ignore_the_owner_and_collapse_onto_one_name() {
        let a = derive_volume_name("data", None, "ws");
        let b = derive_volume_name("data", None, "ws");
        assert_eq!(a, b);
        let private = derive_volume_name("data", Some("db.cart.shop"), "ws");
        assert_ne!(a, private);
    }
}
