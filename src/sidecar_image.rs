//! Gets the `fghj-sidecar` image onto the machine: pulls the published one, or
//! builds from the crate source embedded in `fghjd` (for unreleased versions
//! and offline machines).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use bollard::Docker;
use include_dir::{Dir, include_dir};

use crate::{daemon_log, docker, persistence, web};

static SRC_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/src");
const CARGO_TOML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
const CARGO_LOCK: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock"));
const DOCKERFILE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/sidecar/Dockerfile"));

/// Not configurable: the sidecar must match the daemon's version and speak its
/// `routes.json`. Carrying the registry in the tag keeps one name for pulled
/// and locally built images.
const SIDECAR_REPOSITORY: &str = "ghcr.io/mlnja/fghj-sidecar";

const LOCAL_BUILD_ENV: &str = "FGHJ_SIDECAR_LOCAL_BUILD";

pub fn image_tag() -> String {
    format!("{SIDECAR_REPOSITORY}:{}", env!("CARGO_PKG_VERSION"))
}

/// Present, else pulled, else built from embedded source. A present image is
/// never replaced, so a local build sticks. `FGHJ_SIDECAR_LOCAL_BUILD=1` skips
/// the pull, or iterating on the sidecar would pull over your changes.
pub async fn ensure_built(docker: &Docker) -> Result<()> {
    let tag = image_tag();
    if docker.inspect_image(&tag).await.is_ok() {
        return Ok(());
    }

    if std::env::var_os(LOCAL_BUILD_ENV).is_none_or(|v| v.is_empty()) {
        match docker::pull_image(docker, SIDECAR_REPOSITORY, env!("CARGO_PKG_VERSION")).await {
            Ok(()) => {
                daemon_log::info(format!("fghjd: pulled prebuilt sidecar image {tag}"));
                return Ok(());
            }
            // Expected for an unreleased version; the build is the real answer.
            Err(e) => daemon_log::info(format!(
                "fghjd: no prebuilt sidecar image to pull ({e:#}); building {tag} from source"
            )),
        }
    }

    build_locally(docker, &tag).await
}

async fn build_locally(docker: &Docker, tag: &str) -> Result<()> {
    let scratch = persistence::fghjd_root().join("sidecar-build");
    materialize(&scratch)
        .with_context(|| format!("failed to materialize sidecar build context at {scratch:?}"))?;

    docker::build_image(
        docker,
        &docker::BuildOpts {
            context_dir: &scratch,
            dockerfile: "Dockerfile",
            dockerfile_inline: None,
            skip_git_dir: false,
            tag,
            platform: None,
            args: &BTreeMap::new(),
            target: None,
            secrets: &[],
            ssh_auth_sock: None,
            // Not a user repo, so no `docker build` of it to agree with.
            builder: None,
        },
    )
    .await
    .map(|_| ())
    .context("failed to build fghj-sidecar image")
}

fn materialize(scratch: &Path) -> Result<()> {
    if scratch.exists() {
        std::fs::remove_dir_all(scratch)
            .with_context(|| format!("failed to clear stale {scratch:?}"))?;
    }
    std::fs::create_dir_all(scratch).with_context(|| format!("failed to create {scratch:?}"))?;

    std::fs::write(scratch.join("Cargo.toml"), CARGO_TOML)?;
    std::fs::write(scratch.join("Cargo.lock"), CARGO_LOCK)?;
    std::fs::write(scratch.join("Dockerfile"), DOCKERFILE)?;
    // `include_str!` of `sidecar/Dockerfile` must resolve inside the build too.
    std::fs::create_dir_all(scratch.join("sidecar"))
        .with_context(|| format!("failed to create {:?}", scratch.join("sidecar")))?;
    std::fs::write(scratch.join("sidecar").join("Dockerfile"), DOCKERFILE)?;
    // `extract` doesn't create its own base path, so create both roots first.
    extract_into(&SRC_DIR, &scratch.join("src"), "src/")?;
    extract_into(
        &web::ui::UI_DIST,
        &scratch.join("ui").join("dist"),
        "ui/dist",
    )?;
    Ok(())
}

fn extract_into(dir: &include_dir::Dir<'_>, dest: &Path, label: &str) -> Result<()> {
    std::fs::create_dir_all(dest)
        .with_context(|| format!("failed to create {dest:?} for embedded {label}"))?;
    dir.extract(dest)
        .with_context(|| format!("failed to extract embedded {label} into {dest:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `extract` doesn't create its base path, and the build fallback is all there
    /// is when the pull fails. Checks the files the Dockerfile copies.
    #[test]
    fn materialize_writes_a_build_context_the_dockerfile_can_consume() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("sidecar-build");

        materialize(&scratch).expect("materialize must succeed");

        for expected in [
            "Cargo.toml",
            "Cargo.lock",
            "Dockerfile",
            "sidecar/Dockerfile",
            "src/lib.rs",
            "src/sidecar_image.rs",
            "src/raw_net/macos.rs",
        ] {
            assert!(
                scratch.join(expected).is_file(),
                "{expected} missing from the build context"
            );
        }
        assert!(scratch.join("ui/dist").is_dir(), "ui/dist missing");
    }

    #[test]
    fn materialize_replaces_a_stale_build_context() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("sidecar-build");
        std::fs::create_dir_all(scratch.join("src")).unwrap();
        std::fs::write(scratch.join("src").join("ghost.rs"), "stale").unwrap();

        materialize(&scratch).expect("materialize must succeed");

        assert!(!scratch.join("src").join("ghost.rs").exists());
        assert!(scratch.join("src/lib.rs").is_file());
    }

    /// A bare tag would make the pull target and the requested reference differ,
    /// re-pulling on every start.
    #[test]
    fn image_tag_is_the_published_reference_at_this_crates_version() {
        let tag = image_tag();
        assert_eq!(
            tag,
            format!("{SIDECAR_REPOSITORY}:{}", env!("CARGO_PKG_VERSION"))
        );
    }

    /// Without a dot, colon or `localhost`, Docker reads the first component as a
    /// Docker Hub user, so a dropped `ghcr.io` would pull a stranger's image.
    #[test]
    fn the_repository_names_a_real_registry_host() {
        let (host, rest) = SIDECAR_REPOSITORY
            .split_once('/')
            .expect("repository must be registry-qualified");
        assert!(
            host.contains('.') || host.contains(':') || host == "localhost",
            "'{host}' would be read as a Docker Hub namespace, not a registry"
        );
        assert!(!rest.is_empty());
    }
}
