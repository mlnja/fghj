//! Which BuildKit the host's `docker build` would use. bollard builds on
//! dockerd's embedded BuildKit, but buildx may select a separate one at a
//! different version, and the same Dockerfile then builds in one and fails in
//! the other. Only the `docker-container` driver is followed; `None` means
//! "use the embedded builder".

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostBuilder {
    /// Display only.
    pub builder: String,
    /// The first node, as one `docker build` on one host would use.
    pub node: String,
    pub container: String,
}

/// buildx's convention, not recorded anywhere. If it changes, `bootstrap()`
/// makes a second, cache-cold container instead of failing.
fn container_name(node: &str) -> String {
    format!("buildx_buildkit_{node}")
}

#[derive(Deserialize)]
struct Current {
    #[serde(rename = "Name")]
    name: Option<String>,
}

#[derive(Deserialize)]
struct Instance {
    #[serde(rename = "Driver")]
    driver: Option<String>,
    #[serde(rename = "Nodes")]
    nodes: Option<Vec<Node>>,
}

#[derive(Deserialize)]
struct Node {
    #[serde(rename = "Name")]
    name: Option<String>,
}

/// `home` is the workspace owner's: `fghjd` is root, and root's `HOME` has no
/// buildx config. Every failure is `None`, which degrades to the embedded
/// builder.
pub fn default_builder(home: &Path) -> Option<HostBuilder> {
    let buildx = PathBuf::from(home).join(".docker").join("buildx");

    // An empty `Name` is buildx's "default builder", i.e. dockerd's own.
    let current: Current = read_json(&buildx.join("current"))?;
    let name = current.name.filter(|n| !n.is_empty())?;

    let instance: Instance = read_json(&buildx.join("instances").join(&name))?;
    if instance.driver.as_deref() != Some("docker-container") {
        return None;
    }
    let node = instance
        .nodes
        .unwrap_or_default()
        .into_iter()
        .find_map(|n| n.name)
        .filter(|n| !n.is_empty())?;

    Some(HostBuilder {
        builder: name,
        container: container_name(&node),
        node,
    })
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(current: &str, instances: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let buildx = dir.path().join(".docker").join("buildx");
        std::fs::create_dir_all(buildx.join("instances")).unwrap();
        std::fs::write(buildx.join("current"), current).unwrap();
        for (name, body) in instances {
            std::fs::write(buildx.join("instances").join(name), body).unwrap();
        }
        dir
    }

    const ORB: &str = r#"{"Name":"orb-builder","Driver":"docker-container",
        "Nodes":[{"Name":"orb-builder0","Endpoint":"orbstack"}]}"#;

    /// OrbStack: `docker build` used BuildKit v0.32.2 in a container while fghj
    /// used dockerd's v0.29.0.
    #[test]
    fn a_container_backed_builder_resolves_to_its_buildkit_container() {
        let home = home_with(
            r#"{"Key":"orbstack","Name":"orb-builder","Global":false}"#,
            &[("orb-builder", ORB)],
        );
        assert_eq!(
            default_builder(home.path()),
            Some(HostBuilder {
                builder: String::from("orb-builder"),
                node: String::from("orb-builder0"),
                container: String::from("buildx_buildkit_orb-builder0"),
            })
        );
    }

    #[test]
    fn anything_fghj_cannot_or_need_not_redirect_to_is_none() {
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(default_builder(bare.path()), None);

        let unset = home_with(r#"{"Key":"default","Name":"","Global":false}"#, &[]);
        assert_eq!(default_builder(unset.path()), None);
        let missing = home_with(r#"{"Key":"default"}"#, &[]);
        assert_eq!(default_builder(missing.path()), None);

        let dangling = home_with(r#"{"Name":"ghost"}"#, &[]);
        assert_eq!(default_builder(dangling.path()), None);

        let remote = home_with(
            r#"{"Name":"far"}"#,
            &[(
                "far",
                r#"{"Name":"far","Driver":"remote","Nodes":[{"Name":"far0"}]}"#,
            )],
        );
        assert_eq!(default_builder(remote.path()), None);

        let nodeless = home_with(
            r#"{"Name":"empty"}"#,
            &[(
                "empty",
                r#"{"Name":"empty","Driver":"docker-container","Nodes":[]}"#,
            )],
        );
        assert_eq!(default_builder(nodeless.path()), None);

        let garbage = home_with("{not json", &[]);
        assert_eq!(default_builder(garbage.path()), None);
    }

    #[test]
    fn a_multi_node_builder_takes_its_first_node() {
        let home = home_with(
            r#"{"Name":"farm"}"#,
            &[(
                "farm",
                r#"{"Name":"farm","Driver":"docker-container",
                    "Nodes":[{"Name":"farm0"},{"Name":"farm1"}]}"#,
            )],
        );
        assert_eq!(
            default_builder(home.path()).unwrap().container,
            "buildx_buildkit_farm0"
        );
    }
}
