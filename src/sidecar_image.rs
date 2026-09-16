//! Builds the Docker image for `fghj-sidecar` (see `src/bin/fghj-sidecar.rs`)
//! — the in-network TLS proxy sidecar `runs.rs` starts one of per run.
//!
//! `fghjd` ships as a prebuilt binary with no cargo workspace on the target
//! machine, so the sidecar's Docker build context can't just be "the repo on
//! disk" — the whole crate (`Cargo.toml`/`Cargo.lock`/`src/**`/`ui/dist`) is
//! embedded into `fghjd` at compile time (same `include_dir!` trick
//! `server.rs` already uses for the UI) and materialized to a scratch
//! directory the first time a sidecar image is actually needed.

use std::path::Path;

use anyhow::{Context, Result};
use bollard::Docker;
use include_dir::{Dir, include_dir};

use crate::{docker, server, store};

static SRC_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/src");
const CARGO_TOML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
const CARGO_LOCK: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock"));
const DOCKERFILE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/sidecar/Dockerfile"));

/// Where `runs.rs` starts sidecar containers from — pinned to this build's
/// own crate version so a `fghjd` upgrade always builds (and uses) a
/// matching sidecar rather than silently reusing a stale one left by an
/// older version.
pub fn image_tag() -> String {
    format!("fghj-sidecar:{}", env!("CARGO_PKG_VERSION"))
}

/// Builds the sidecar image if `image_tag()` doesn't already exist locally —
/// called once, near daemon startup, not per run-start. Known limitation:
/// iterating on the sidecar's own source during development won't be picked
/// up without bumping the crate version or manually removing the cached
/// image tag.
pub async fn ensure_built(docker: &Docker) -> Result<()> {
    let tag = image_tag();
    if docker.inspect_image(&tag).await.is_ok() {
        return Ok(());
    }

    let scratch = store::fghjd_root().join("sidecar-build");
    materialize(&scratch)
        .with_context(|| format!("failed to materialize sidecar build context at {scratch:?}"))?;

    docker::build_image(docker, &scratch, "Dockerfile", &tag, None)
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
    server::UI_DIST
        .extract(scratch.join("ui/dist"))
        .context("failed to extract embedded ui/dist into sidecar build context")?;
    Ok(())
}
