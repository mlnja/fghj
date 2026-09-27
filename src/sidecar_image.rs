//! Gets the Docker image for `fghj-sidecar` (see `src/bin/fghj-sidecar.rs`)
//! — the in-network TLS proxy sidecar `runs.rs` starts one of per run — onto
//! the machine, by pulling the published build or, failing that, building it.
//!
//! **Pulling** is the normal path for an installed release: the release
//! workflow builds `sidecar/Dockerfile` for `linux/amd64` and `linux/arm64`
//! and pushes the result to [`SIDECAR_REPOSITORY`] under the crate version.
//!
//! **Building** is the fallback, and it can't just use "the repo on disk":
//! `fghjd` ships as a prebuilt binary with no cargo workspace on the target
//! machine. So the whole crate
//! (`Cargo.toml`/`Cargo.lock`/`src/**`/`ui/dist`) is embedded into `fghjd` at
//! compile time (same `include_dir!` trick `web::ui` already uses for the UI)
//! and materialized to a scratch directory when a sidecar image is actually
//! needed. That keeps `cargo run` from an unreleased working tree — and an
//! offline machine — working exactly as before.

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

/// Where the release workflow publishes the prebuilt sidecar image, and so
/// also the reference `runs.rs` starts sidecar containers from.
///
/// The registry is baked into the binary rather than configurable because this
/// is not a user-chosen image: it is fghj's own proxy, whose source lives in
/// this very crate and whose version must match the daemon's exactly. A knob
/// here would only let someone point the daemon at a sidecar that doesn't
/// speak the same `routes.json`.
///
/// Carrying the registry in the tag (rather than pulling and then re-tagging
/// to a bare `fghj-sidecar:x.y.z`) means there is exactly one string for a
/// sidecar image anywhere in the system — `docker images` shows the pulled
/// image under the same name a local build produces, so `ensure_built`'s
/// "already present?" check cannot disagree with what `runs.rs` asks for.
const SIDECAR_REPOSITORY: &str = "ghcr.io/mlnja/fghj-sidecar";

/// Set to any non-empty value to skip the pull and always build from the
/// embedded source. See `ensure_built`.
const LOCAL_BUILD_ENV: &str = "FGHJ_SIDECAR_LOCAL_BUILD";

/// Pinned to this build's own crate version so a `fghjd` upgrade always uses a
/// matching sidecar rather than silently reusing a stale one left by an older
/// version.
pub fn image_tag() -> String {
    format!("{SIDECAR_REPOSITORY}:{}", env!("CARGO_PKG_VERSION"))
}

/// Makes `image_tag()` available locally — called once near daemon startup and
/// again from `RunRegistry::ensure_sidecar`, so it has to be cheap when there
/// is nothing to do.
///
/// Three paths, in order:
///
/// 1. **Already present.** Nothing to do. This is also what makes the local
///    escape hatch stick: an image you built yourself is indistinguishable
///    from a pulled one here, so the daemon won't go behind your back and
///    replace it on the next start.
/// 2. **Pull the published image.** The release workflow builds this exact
///    Dockerfile for `linux/amd64` and `linux/arm64` and pushes it to GHCR
///    under the crate version, so a user who installed a released `fghjd`
///    downloads a ~100MB debian-slim image instead of compiling the whole
///    crate inside a container. That compile was the single slowest thing
///    about a first run, and the one most likely to fail for reasons that have
///    nothing to do with the user's own project.
/// 3. **Build from the embedded source.** The original behavior, kept as the
///    fallback rather than deleted, because the pull can legitimately fail:
///    a developer running `cargo run` at a version that was never released, an
///    offline machine, a fork that doesn't publish to this registry. None of
///    those should be fatal when the source to build is right here.
///
/// Set `FGHJ_SIDECAR_LOCAL_BUILD=1` to skip step 2 outright. Without it,
/// iterating on the sidecar's own source is a trap: you'd remove the cached
/// tag to force a rebuild and get the published image pulled over your
/// changes instead.
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
            // Not a warning: for an unreleased version this is the expected
            // path, and the build that follows is the real answer. If *it*
            // fails, its own error is what the caller reports.
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
            tag,
            platform: None,
            args: &BTreeMap::new(),
            target: None,
            secrets: &[],
            ssh_auth_sock: None,
        },
    )
    .await
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
    // Also placed under `sidecar/` in the build context, matching this
    // crate's own layout — the Dockerfile's own `COPY sidecar ./sidecar`
    // step needs it there, since `cargo build --release --bin fghj-sidecar`
    // recompiles the whole `fghj` lib crate (this module included) and its
    // `include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/sidecar/Dockerfile"))`
    // must resolve inside that build too.
    std::fs::create_dir_all(scratch.join("sidecar"))
        .with_context(|| format!("failed to create {:?}", scratch.join("sidecar")))?;
    std::fs::write(scratch.join("sidecar").join("Dockerfile"), DOCKERFILE)?;
    SRC_DIR
        .extract(scratch.join("src"))
        .context("failed to extract embedded src/ into sidecar build context")?;
    web::ui::UI_DIST
        .extract(scratch.join("ui/dist"))
        .context("failed to extract embedded ui/dist into sidecar build context")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the "one string for a sidecar image" invariant. If this ever
    /// went back to a bare `fghj-sidecar:x.y.z`, `ensure_built`'s pull target
    /// and the reference `runs.rs` asks Docker for would quietly disagree:
    /// the pull would land under a registry-qualified name, the inspect would
    /// keep missing, and every start would re-pull.
    #[test]
    fn image_tag_is_the_published_reference_at_this_crates_version() {
        let tag = image_tag();
        assert_eq!(
            tag,
            format!("{SIDECAR_REPOSITORY}:{}", env!("CARGO_PKG_VERSION"))
        );
    }

    /// Docker only treats a reference's first component as a registry host if
    /// it *looks* like one — it needs a dot, a colon, or to be `localhost`.
    /// `mlnja/fghj-sidecar` is a legal reference, but it names a Docker Hub
    /// repository, so a typo that dropped `ghcr.io` wouldn't fail loudly; it
    /// would pull a stranger's image, or someone else's future squatted one.
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
