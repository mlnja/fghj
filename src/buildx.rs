//! Which BuildKit the host's `docker build` would actually use.
//!
//! fghj builds through the Docker Engine API, and bollard's `Moby` driver
//! points that at the BuildKit *embedded in dockerd*. The `docker` CLI does
//! not: since buildx became the default front end, `docker build` resolves a
//! **builder** from `~/.docker/buildx` and may well land on a completely
//! separate BuildKit — commonly one running in a container, at a different
//! version from dockerd's.
//!
//! That divergence is invisible and expensive. The same Dockerfile can build
//! on the command line and fail under fghj (or the reverse) purely because two
//! BuildKit minor versions disagree, and nothing in either error mentions that
//! two engines were involved. So fghj reads the same config the CLI reads and
//! builds where the CLI would.
//!
//! Only the `docker-container` driver is resolvable here. `docker` (dockerd's
//! own embedded BuildKit) is what fghj does anyway, and `remote`/`kubernetes`
//! name endpoints fghj has no credentials for — all three return `None`, which
//! means "use the embedded builder" rather than "something went wrong".

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A buildx builder backed by a BuildKit container on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostBuilder {
    /// The builder name as `docker buildx ls` shows it, e.g. `orb-builder`.
    /// Display only — it is what a human would type, not what Docker knows.
    pub builder: String,
    /// The node within that builder. A builder can have several (that is how
    /// buildx does multi-arch across machines); fghj takes the first, because
    /// a single `docker build` on one host lands on one node too.
    pub node: String,
    /// The container actually running BuildKit. buildx derives this from the
    /// node name by a fixed convention rather than recording it, so this is
    /// reconstructed the same way — see [`container_name`].
    pub container: String,
}

/// buildx's own naming convention for the container behind a node.
///
/// Hardcoded in buildx (`driver/docker-container/factory.go`) rather than
/// stored in the instance file, so matching it is the only way to find the
/// container. If buildx ever changes it, `bootstrap()` would create a second
/// BuildKit container beside the real one instead of failing — slower and
/// cache-cold, but still correct, which is the right way for this to break.
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

/// The builder `docker build` would use, for the user whose `.docker` lives
/// under `home`.
///
/// `home` is the **workspace owner's**, never the process's: `fghjd` runs as
/// root, so `$HOME` here is `/var/root` and reading it would find no buildx
/// config at all — then silently conclude the CLI uses the embedded builder,
/// which is precisely the wrong answer and exactly the shape of bug this
/// function exists to remove. See `WorkspaceOwner::home`.
///
/// Every failure is `None`: no config (buildx never used), an unset builder
/// (the CLI uses dockerd's embedded BuildKit, same as fghj), a driver fghj
/// can't reach, or malformed JSON. None of those is worth failing a build
/// over, because `None` degrades to today's behaviour.
pub fn default_builder(home: &Path) -> Option<HostBuilder> {
    let buildx = PathBuf::from(home).join(".docker").join("buildx");

    // An absent or empty `Name` is buildx's own encoding of "the default
    // builder", which is dockerd's embedded BuildKit — nothing to redirect.
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

    /// Lays out a `.docker/buildx` tree the way buildx does, so the tests
    /// exercise path joining and not just the JSON shapes.
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

    /// The real files from a machine running OrbStack, which is the case that
    /// prompted all of this: `docker build` went to BuildKit v0.32.2 in a
    /// container while fghj used dockerd's embedded v0.29.0.
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

    /// All of these mean "build where you already build", so they must be
    /// indistinguishable from each other at the call site.
    #[test]
    fn anything_fghj_cannot_or_need_not_redirect_to_is_none() {
        // buildx has never run: no config at all.
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(default_builder(bare.path()), None);

        // Explicitly the default builder — dockerd's embedded BuildKit, which
        // is what fghj uses already.
        let unset = home_with(r#"{"Key":"default","Name":"","Global":false}"#, &[]);
        assert_eq!(default_builder(unset.path()), None);
        let missing = home_with(r#"{"Key":"default"}"#, &[]);
        assert_eq!(default_builder(missing.path()), None);

        // Selected builder names an instance file that isn't there.
        let dangling = home_with(r#"{"Name":"ghost"}"#, &[]);
        assert_eq!(default_builder(dangling.path()), None);

        // A driver fghj has no way to reach.
        let remote = home_with(
            r#"{"Name":"far"}"#,
            &[(
                "far",
                r#"{"Name":"far","Driver":"remote","Nodes":[{"Name":"far0"}]}"#,
            )],
        );
        assert_eq!(default_builder(remote.path()), None);

        // Right driver, but no node to derive a container name from.
        let nodeless = home_with(
            r#"{"Name":"empty"}"#,
            &[(
                "empty",
                r#"{"Name":"empty","Driver":"docker-container","Nodes":[]}"#,
            )],
        );
        assert_eq!(default_builder(nodeless.path()), None);

        // Truncated file — a build must not fail over this.
        let garbage = home_with("{not json", &[]);
        assert_eq!(default_builder(garbage.path()), None);
    }

    /// A builder with several nodes takes the first, matching what one
    /// `docker build` invocation on one host does.
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
