//! Getting images into the local store: pulls, and builds — classic
//! `docker build` or BuildKit, embedded or on the host's buildx builder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bollard::Docker;
use bollard::grpc::error::GrpcError;
use bollard::query_parameters::CreateImageOptionsBuilder;
use futures_util::StreamExt;

/// Everything one `docker build` needs that isn't the tar stream itself.
///
/// Grouped into a struct rather than passed positionally because the list is
/// open-ended: `#Build` is the one part of the language that mirrors a whole
/// foreign CLI, and every field added here has to reach the daemon or it is
/// silently ignored — which is exactly what happened to `args` before
/// `concepts/build-inputs.md` was written.
pub struct BuildOpts<'a> {
    pub context_dir: &'a Path,
    pub dockerfile: &'a str,
    /// `#Build.dockerfile_inline`: sent in the context tar as
    /// [`INLINE_DOCKERFILE`] and built from, so nothing is written to disk.
    pub dockerfile_inline: Option<&'a str>,
    /// Leave the context's `.git` out, whatever `.dockerignore` says — for a
    /// git build context, whose clone's history is fghj's, not the build's.
    /// This is what BuildKit does with a git URL context too.
    pub skip_git_dir: bool,
    pub tag: &'a str,
    pub platform: Option<&'a str>,
    /// `#Build.args` — `docker build --build-arg`. Only the `ARG`s a
    /// Dockerfile actually declares have any effect; Docker ignores the rest.
    pub args: &'a BTreeMap<String, String>,
    /// `#Build.target` — which stage of a multi-stage Dockerfile to stop at.
    pub target: Option<&'a str>,
    /// `#Build.secrets`, already resolved to absolute host paths — each entry
    /// is the `id` a `RUN --mount=type=secret,id=…` names and the file whose
    /// bytes BuildKit should serve for it.
    pub secrets: &'a [(String, PathBuf)],
    /// The ssh-agent socket to forward as BuildKit's `default` ssh socket,
    /// for a Dockerfile doing `RUN --mount=type=ssh`. This is a resolved,
    /// *live* path (`WorkspaceOwner`'s, via `live_ssh_auth_sock`) rather than
    /// the `#Build.ssh` boolean, because `fghjd` runs as root and has no
    /// agent of its own to forward — the whole point is to lend it the
    /// workspace owner's. `None` forwards nothing.
    pub ssh_auth_sock: Option<&'a str>,
    /// Which BuildKit to build on: the host's own buildx builder, or `None`
    /// for the one embedded in dockerd.
    ///
    /// `None` is not "no preference" — it is dockerd's embedded BuildKit,
    /// which is a *different engine* from the one `docker build` reaches
    /// whenever buildx has a container-backed builder selected. The two run
    /// different BuildKit versions and disagree about real Dockerfiles, so
    /// resolving this (see `crate::buildx::default_builder`) is what stops
    /// `fghj` and `docker build` giving different answers about the same
    /// repo.
    pub builder: Option<&'a crate::buildx::HostBuilder>,
}

/// Serializes the process-global `SSH_AUTH_SOCK` mutation that BuildKit ssh
/// forwarding requires.
///
/// bollard's `SshProvider` reads the agent socket from *this process's*
/// environment (`bollard::grpc::mod.rs`, `check_agent`) with no way to pass
/// one in. `fghjd` is a root daemon with no agent of its own, so the only way
/// to lend it the workspace owner's is to set the variable around the build
/// — which is global to the process, hence the lock. Held across the whole
/// build, so two ssh-forwarding builds never overlap; builds that don't
/// forward ssh never touch it and stay fully concurrent.
static SSH_AUTH_SOCK_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Pulls `repository:tag` from whatever registry `repository` names, leaving
/// it in the local image store under that same reference.
///
/// The only caller today is `sidecar_image::ensure_built` — fghj does not pull
/// the images its *nodes* declare (see `RunOpts.platform`'s note), because a
/// node's image may be private and the credential story for that is the
/// workspace owner's docker config, not fghjd's. The sidecar is different: it
/// is fghj's own image, published publicly, so an anonymous pull is always
/// enough and no `DockerCredentials` are passed.
///
/// A pull that the daemon reports as failed mid-stream (unknown manifest, no
/// network, registry 5xx) arrives as a stream item that bollard has already
/// turned into an `Err` from `error_detail` — so draining the stream and
/// propagating the first error is the whole implementation. Progress lines are
/// deliberately discarded: the one caller is a startup step whose slowness is
/// already explained by `bootstrap.rs`, and nothing in the UI polls it.
pub async fn pull_image(docker: &Docker, repository: &str, tag: &str) -> Result<()> {
    let options = CreateImageOptionsBuilder::default()
        .from_image(repository)
        .tag(tag)
        .build();
    let mut stream = docker.create_image(Some(options), None, None);
    while let Some(item) = stream.next().await {
        item.with_context(|| format!("failed to pull {repository}:{tag}"))?;
    }
    Ok(())
}

/// How big the build context was, so the events pane can say it.
///
/// A context is tarred and uploaded in full on every build, so a stray
/// `node_modules` or `.git` inside it is a silent per-build cost that
/// nothing else in fghj would ever mention.
#[derive(Debug)]
pub struct BuildReport {
    pub context_bytes: u64,
    /// The buildx builder this ran on, or `None` for dockerd's embedded
    /// BuildKit. Reported so the events pane can name the engine: when a build
    /// behaves differently from the same `docker build` on the command line,
    /// which BuildKit ran it is the first thing worth knowing and the one
    /// thing no error message mentions.
    pub builder: Option<String>,
}

/// Turns a BuildKit solve failure into something an operator can act on.
///
/// The raw error arrives saying the same thing three times. `GrpcError`'s
/// `TonicStatus` variant renders as `status = {code}, message = {message}`
/// *and*, because thiserror's `#[from]` also makes the status its `source`,
/// anyhow's `{:#}` chain walks into it and prints the same message again —
/// so a one-line Go compile failure reaches the events pane as 300-odd
/// characters of which about 60 are information:
///
/// ```text
/// buildkit build of fghj/x:local failed: Grpc response failure: status =
/// Unknown error, message = process "/bin/sh -c go build ..." did not
/// complete successfully: exit code: 1: code: 'Unknown error', message:
/// "process \"/bin/sh -c go build ...\" did not complete successfully: exit
/// code: 1"
/// ```
///
/// This takes the status message once and splits it into the two facts it
/// actually carries — which step, and what it exited with — then says where
/// the step's own output is, because that is the next thing anyone reads
/// this message wanting.
///
/// The `code` is dropped on purpose: BuildKit reports every failed build
/// step as `Unknown`, so it distinguishes nothing while costing a line.
fn describe_solve_failure(opts: &BuildOpts<'_>, err: GrpcError) -> anyhow::Error {
    let GrpcError::TonicStatus { err: status } = &err else {
        // Transport, UTF-8 and metadata failures aren't a failing build
        // step, so there is nothing to take apart — keep the original.
        return anyhow::Error::from(err).context(format!("buildkit build of {} failed", opts.tag));
    };

    let message = status.message().trim().to_string();
    let (step, exit) = split_step_failure(&message);

    let mut out = format!("build of {} failed", opts.tag);
    match (step, exit) {
        (Some(step), Some(exit)) => {
            out.push_str(&format!("\nfailing step: {step}\nexit code: {exit}"));
        }
        // Not the `process "..." did not complete successfully` shape — a
        // missing base image, an unparseable Dockerfile, a secret that
        // isn't there. Those messages are already a sentence, so they go
        // through whole rather than being forced into fields.
        _ => out.push_str(&format!("\n{message}")),
    }

    out.push_str(&format!(
        "\n\nStep output is not captured: BuildKit streams it over a gRPC \
         channel the Docker API client fghjd uses does not expose. To see it:\
         \n  docker build -f {} {}",
        opts.dockerfile,
        opts.context_dir.display()
    ));

    anyhow::Error::msg(out)
}

/// Pulls the failing command and its exit code out of BuildKit's standard
/// step-failure message:
///
/// ```text
/// process "/bin/sh -c go build ./..." did not complete successfully: exit code: 1
/// ```
///
/// Returns `(None, None)` for anything else, which the caller passes through
/// untouched rather than guessing at.
fn split_step_failure(message: &str) -> (Option<&str>, Option<&str>) {
    const MARKER: &str = "\" did not complete successfully: exit code: ";
    let Some(rest) = message.strip_prefix("process \"") else {
        return (None, None);
    };
    let Some(split) = rest.find(MARKER) else {
        return (None, None);
    };
    let step = &rest[..split];
    let exit = rest[split + MARKER.len()..].trim();
    if step.is_empty() || exit.is_empty() {
        return (None, None);
    }
    (Some(step), Some(exit))
}

/// Builds `opts.tag` from `opts.context_dir`, always through BuildKit.
///
/// There used to be a second, classic-builder path taken by any build that
/// declared neither `secrets` nor `ssh`, because bollard's BuildKit driver
/// gives up the classic path's per-step `error_detail.message` stream. That
/// split is gone: BuildKit has been Docker's default builder since Engine
/// 23.0, and keeping two builders meant `RUN --mount=type=cache` — the
/// single most useful BuildKit feature for a dev loop — worked only in
/// repos that happened to declare a secret for unrelated reasons. One
/// builder, and a daemon too old to offer it is a failed build with a clear
/// message rather than a silently different one. `fghj doctor` checks for it
/// up front; see `concepts/build-inputs.md`.
pub async fn build_image(docker: &Docker, opts: &BuildOpts<'_>) -> Result<BuildReport> {
    let tar_bytes = tar_build_context(
        opts.context_dir,
        opts.dockerfile,
        opts.dockerfile_inline,
        opts.skip_git_dir,
    )
    .await?;
    let context_bytes = tar_bytes.len() as u64;
    build_image_buildkit(docker, opts, tar_bytes).await?;
    Ok(BuildReport {
        context_bytes,
        builder: opts.builder.map(|b| b.builder.clone()),
    })
}

/// The name an inline Dockerfile has in the context tar. Reserved: a file of
/// this name in the context is replaced.
pub const INLINE_DOCKERFILE: &str = ".fghj.Dockerfile";

async fn tar_build_context(
    context_dir: &Path,
    dockerfile: &str,
    inline: Option<&str>,
    skip_git_dir: bool,
) -> Result<Vec<u8>> {
    let context_dir = context_dir.to_path_buf();
    let dockerfile = dockerfile.to_string();
    let inline = inline.map(str::to_string);
    tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let ignore = if skip_git_dir {
            // Last, so no `!` pattern above it can bring `.git` back.
            let text =
                std::fs::read_to_string(context_dir.join(".dockerignore")).unwrap_or_default();
            crate::dockerignore::Dockerignore::parse(&format!("{text}\n.git\n"))
        } else {
            crate::dockerignore::Dockerignore::load(&context_dir)
        };
        let mut builder = tar::Builder::new(Vec::new());
        if ignore.is_empty() {
            builder.append_dir_all("", &context_dir).with_context(|| {
                format!("failed to tar build context {}", context_dir.display())
            })?;
        } else {
            append_filtered(&mut builder, &context_dir, &ignore, &dockerfile)?;
        }
        if let Some(inline) = inline {
            let mut header = tar::Header::new_gnu();
            header.set_size(inline.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, INLINE_DOCKERFILE, inline.as_bytes())
                .context("failed to add the inline Dockerfile to the build context")?;
        }
        builder
            .into_inner()
            .context("failed to finalize build context tar")
    })
    .await
    .context("tar task panicked")?
}

/// Walks the context, leaving out what `.dockerignore` excludes.
///
/// Separate from the unfiltered path above so a context with no
/// `.dockerignore` keeps using `tar`'s own bulk walk, which is both faster and
/// the behaviour every existing build already has.
///
/// **The Dockerfile and `.dockerignore` are always sent, even when the file
/// excludes them.** This is not a nicety: real `.dockerignore` files do list
/// `Dockerfile` — it has no business inside the image, and `COPY . .` would
/// otherwise bake it in — and honouring that literally would leave BuildKit
/// with no Dockerfile to build and turn a context fix into a broken build.
/// The Docker CLI makes the same exception.
fn append_filtered(
    builder: &mut tar::Builder<Vec<u8>>,
    root: &Path,
    ignore: &crate::dockerignore::Dockerignore,
    dockerfile: &str,
) -> Result<()> {
    let always = [dockerfile.trim_start_matches("./"), ".dockerignore"];
    // Depth-first over a stack of directories still to read. A directory's own
    // header is appended before it is pushed, so a parent always precedes its
    // children in the archive.
    let mut stack = vec![String::new()];
    while let Some(dir) = stack.pop() {
        let abs_dir = if dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&dir)
        };
        let entries = std::fs::read_dir(&abs_dir)
            .with_context(|| format!("failed to read build context {}", abs_dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("failed to read {}", abs_dir.display()))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let rel = if dir.is_empty() {
                name.to_string()
            } else {
                format!("{dir}/{name}")
            };
            let abs = entry.path();
            // `metadata`, not `file_type`: it follows symlinks, which is what
            // `append_dir_all` does. A broken link is skipped rather than
            // failing the whole build.
            let Ok(meta) = std::fs::metadata(&abs) else {
                continue;
            };
            let is_dir = meta.is_dir();
            if !always.contains(&rel.as_str()) && ignore.excludes(&rel) {
                // An excluded directory is normally not even opened — that is
                // what keeps a 232 MB `node_modules` from costing anything. It
                // has to be walked only when some `!` pattern could re-include
                // a file inside it, and even then its own entry stays out.
                if is_dir && ignore.has_exclusions() {
                    stack.push(rel);
                }
                continue;
            }
            if is_dir {
                builder
                    .append_dir(&rel, &abs)
                    .with_context(|| format!("failed to tar {}", abs.display()))?;
                stack.push(rel);
            } else {
                builder
                    .append_path_with_name(&abs, &rel)
                    .with_context(|| format!("failed to tar {}", abs.display()))?;
            }
        }
    }
    Ok(())
}

/// One BuildKit solve, with no progress stream: bollard's
/// `Build::docker_build` runs the whole thing and returns
/// `Result<(), GrpcError>`. The image lands in the daemon's local image store
/// under `tag`, because the `Moby` driver asks for the `docker` exporter, so
/// everything downstream (`create_container`, `spec_hash`'s image tag) is
/// unchanged.
async fn build_image_buildkit(
    docker: &Docker,
    opts: &BuildOpts<'_>,
    tar_bytes: Vec<u8>,
) -> Result<()> {
    use bollard::grpc::build::{ImageBuildFrontendOptions, ImageBuildLoadInput, SecretSource};
    let mut frontend = ImageBuildFrontendOptions::builder().dockerfile(Path::new(opts.dockerfile));
    if let Some(target) = opts.target {
        frontend = frontend.target(target);
    }
    for (key, value) in opts.args {
        frontend = frontend.buildarg(key, value);
    }
    if let Some(platform) = opts.platform {
        frontend = frontend.platforms(&parse_platform(platform)?);
    }
    for (id, path) in opts.secrets {
        // `SecretSource::Env` exists too, and is deliberately unreachable:
        // there is no host environment to read one from until
        // `concepts/AUDIT.md` E2 closes. See `#BuildSecret`.
        frontend = frontend.set_secret(id, &SecretSource::File(PathBuf::from(path)));
    }
    frontend = frontend.enable_ssh(opts.ssh_auth_sock.is_some());
    let frontend = frontend.build();

    let load = ImageBuildLoadInput::Upload(bytes::Bytes::from(tar_bytes));
    let tag = opts.tag;

    let solve = async |frontend, load| match opts.builder {
        Some(builder) => solve_on_host_builder(docker, builder, tag, frontend, load).await,
        None => solve_on_embedded(docker, tag, frontend, load).await,
    };
    let result = match opts.ssh_auth_sock {
        // The guard is scoped to the match arm so the process-global variable
        // is restored — and the lock released — the moment this one build
        // finishes, not at the end of the function.
        Some(sock) => {
            let _guard = SSH_AUTH_SOCK_ENV.lock().await;
            let previous = std::env::var_os("SSH_AUTH_SOCK");
            set_ssh_auth_sock(Some(sock));
            let result = solve(frontend, load).await;
            set_ssh_auth_sock(previous.as_ref().and_then(|v| v.to_str()));
            result
        }
        None => solve(frontend, load).await,
    };
    // Two layers: the outer is this side failing to run the solve at all
    // (runtime, thread), the inner is BuildKit rejecting the build. Only the
    // inner one has a failing step worth taking apart.
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(grpc)) => Err(describe_solve_failure(opts, grpc)),
        Err(e) => Err(e.context(format!("buildkit build of {tag} failed"))),
    }
}

/// Runs one `!Send` BuildKit operation on a thread of its own.
///
/// bollard's solve futures are `!Send` — the driver tear-down handler is a bare
/// `Box<dyn Future>` — so awaiting one inline would make every caller up to and
/// including `tokio::spawn` in `daemon::reconcile` `!Send` too. Rather than
/// restructure the daemon around one dependency's boxed future, the operation
/// gets a current-thread runtime and a `LocalSet` on a dedicated thread, and
/// only its result crosses back.
///
/// `make` rather than a future, because the future itself cannot cross the
/// thread boundary: it is built on the far side, where the `LocalSet` is.
async fn on_dedicated_thread<F, Fut>(make: F) -> Result<Result<(), GrpcError>>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), GrpcError>>,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let outcome = (|| -> Result<Result<(), GrpcError>> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("failed to build a runtime for the buildkit solve")?;
            let local = tokio::task::LocalSet::new();
            Ok(local.block_on(&runtime, make()))
        })();
        let _ = tx.send(outcome);
    });
    rx.await.context("buildkit solve thread panicked")?
}

/// Solves on dockerd's **embedded** BuildKit, which lands the image directly in
/// the local image store under `tag` via the `docker` exporter.
async fn solve_on_embedded(
    docker: &Docker,
    tag: &str,
    frontend: bollard::grpc::build::ImageBuildFrontendOptions,
    load: bollard::grpc::build::ImageBuildLoadInput,
) -> Result<Result<(), GrpcError>> {
    use bollard::grpc::driver::Build;
    use bollard::grpc::driver::moby::Moby;

    let docker = docker.clone();
    let tag = tag.to_string();
    on_dedicated_thread(move || async move {
        Moby::new(&docker)
            .docker_build(&tag, frontend, load, None, None)
            .await
    })
    .await
}

/// The BuildKit container fghj runs its builds in.
///
/// Fixed, so the state volume bollard derives from it
/// (`fghj_buildkit_state`) persists and every build after the first sees a warm
/// cache. Deliberately *not* buildx's own container: bollard stamps ownership
/// labels on the containers and volumes it manages and refuses to adopt one it
/// did not create, which is the right guard — stopping or reconfiguring the
/// BuildKit the user's own `docker build` depends on is not fghj's to do.
const FGHJ_BUILDKIT_CONTAINER: &str = "fghj_buildkit";

/// Solves on a BuildKit container of fghj's own, running the **same image** as
/// the host's buildx builder, then loads the result into dockerd.
///
/// Running the same image is the entire point. fghj's builds otherwise go to the
/// BuildKit embedded in dockerd, while `docker build` goes to whatever buildx
/// selected — commonly a container several BuildKit minor versions ahead. Two
/// engines that disagree about the same Dockerfile, with nothing in either error
/// saying so. Taking the image reference straight off buildx's own container
/// means fghj runs the version the user's CLI already proved their Dockerfile
/// against.
///
/// Two costs, both real:
///
/// - **A second BuildKit and a second cache.** fghj cannot share buildx's state
///   volume (see [`FGHJ_BUILDKIT_CONTAINER`]), so the first build of a repo
///   under fghj is cold even if the CLI just built it. Later ones are warm.
/// - **Every built image crosses the socket twice.** bollard implements `Build`
///   — the trait whose `docker` exporter writes straight into dockerd's image
///   store — only for drivers that *are* dockerd. A BuildKit in a container has
///   no access to that store, which is why buildx itself needs `--load`, so the
///   image comes back as a tarball and goes to `/images/load`.
///
/// Both are paid to stop fghj and `docker build` giving different answers about
/// the same repo, which is what was actually broken.
async fn solve_on_host_builder(
    docker: &Docker,
    builder: &crate::buildx::HostBuilder,
    tag: &str,
    frontend: bollard::grpc::build::ImageBuildFrontendOptions,
    load: bollard::grpc::build::ImageBuildLoadInput,
) -> Result<Result<(), GrpcError>> {
    use bollard::grpc::driver::Export;
    use bollard::grpc::driver::ImageExporterEnum;
    use bollard::grpc::driver::docker_container::{
        DockerContainerBuilder, DockerContainerLifecycle,
    };
    use bollard::grpc::export::ImageExporterOutput;

    // Written where fghjd's own state lives rather than `/tmp`: an image
    // tarball is the size of the image, and `/tmp` on macOS is small enough
    // that a large build would fail there for reasons having nothing to do
    // with the build.
    let dir = tempfile::Builder::new()
        .prefix("fghj-build-")
        .tempdir()
        .context("failed to create a directory for the exported image")?;
    let tar = dir.path().join("image.tar");

    let image = buildkit_image_of(docker, &builder.container).await;
    if let Some(image) = &image {
        retire_builder_if_image_changed(docker, image).await;
    }

    let outcome = {
        let docker = docker.clone();
        let tag = tag.to_string();
        let tar = tar.clone();
        on_dedicated_thread(move || async move {
            // A fixed `name` is what makes the cache survive: `bootstrap`
            // reuses the container if it is already there, and the state volume
            // is derived from the name. `Persistent` is already the default;
            // stated anyway, because the alternative — removing the builder
            // after every solve — would silently turn every build cold.
            let mut driver = DockerContainerBuilder::new(&docker);
            driver.name(FGHJ_BUILDKIT_CONTAINER);
            driver.lifecycle(DockerContainerLifecycle::Persistent);
            if let Some(image) = &image {
                driver.image(image);
            }
            let driver = driver.bootstrap().await?;

            let request = ImageExporterOutput::builder(&tag).dest(&tar);
            driver
                .export(
                    ImageExporterEnum::Docker(request),
                    frontend,
                    load,
                    None,
                    None,
                )
                .await
        })
        .await?
    };
    if let Err(e) = outcome {
        return Ok(Err(e));
    }

    load_image_tar(docker, &tar).await?;
    Ok(Ok(()))
}

/// The BuildKit image buildx's own builder is running, as a reference that pins
/// it exactly, so fghj can run the identical build engine.
///
/// A **repo digest** (`moby/buildkit@sha256:…`), not the tag the container was
/// created from. The tag is the obvious choice and it is wrong: buildx's default
/// is `moby/buildkit:buildx-stable-1`, which moves. A container created from it
/// a week ago runs v0.32.2 while pulling the same tag today gets v0.33.1 — so
/// matching tags would leave fghj on a *different* BuildKit from the CLI while
/// looking, by every string comparison available, like it had succeeded. That is
/// the original bug with extra steps.
///
/// Falls back to the tag, then to bollard's own default, because a digest is not
/// always available — an image built locally or loaded from a tarball has no
/// `RepoDigests`. Each step is a weaker guarantee, never a failed build: the
/// worst case is the version drift this started with.
async fn buildkit_image_of(docker: &Docker, container: &str) -> Option<String> {
    let config = docker
        .inspect_container(
            container,
            None::<bollard::query_parameters::InspectContainerOptions>,
        )
        .await
        .ok()?;
    let tag = config
        .config
        .and_then(|c| c.image)
        .filter(|i| !i.is_empty());

    // `image` is the local image *id*, which no registry can be asked for by
    // name; its repo digest is the same bytes expressed as something pullable.
    let digest = match config.image.filter(|i| !i.is_empty()) {
        Some(id) => docker
            .inspect_image(&id)
            .await
            .ok()
            .and_then(|i| i.repo_digests)
            .and_then(|d| d.into_iter().next()),
        None => None,
    };
    digest.or(tag)
}

/// Removes fghj's BuildKit container when it is running a different image from
/// the one it should be.
///
/// `bootstrap` reuses a container by name and never looks at its image, so
/// without this fghj would keep whatever BuildKit it first created forever —
/// including after the host's buildx builder was upgraded underneath it. Since
/// the entire purpose is to match that builder, silently not matching it is the
/// one failure mode worth spending a container teardown to avoid.
///
/// The state volume is deliberately left in place, so the layer cache survives
/// the swap. Best-effort throughout: if any of it fails, `bootstrap` reuses the
/// existing container and the build still succeeds on the older engine.
async fn retire_builder_if_image_changed(docker: &Docker, desired: &str) {
    let current = docker
        .inspect_container(
            FGHJ_BUILDKIT_CONTAINER,
            None::<bollard::query_parameters::InspectContainerOptions>,
        )
        .await
        .ok()
        .and_then(|c| c.config)
        .and_then(|c| c.image);
    if current.as_deref() == Some(desired) || current.is_none() {
        return;
    }

    crate::daemon_log::info(format!(
        "fghjd: rebuilding the fghj buildkit container on {desired} (was {}) \
         to stay on the same engine as `docker build`",
        current.unwrap_or_else(|| String::from("unknown"))
    ));
    let _ = docker
        .remove_container(
            FGHJ_BUILDKIT_CONTAINER,
            Some(
                bollard::query_parameters::RemoveContainerOptionsBuilder::default()
                    .force(true)
                    // The cache lives in the state volume; keep it.
                    .v(false)
                    .build(),
            ),
        )
        .await;
}

/// Feeds a docker-format image tarball to `/images/load`.
///
/// Streamed from disk rather than read into memory: this is a whole image, and
/// `build_image` has already held one full copy of the build context.
async fn load_image_tar(docker: &Docker, tar: &Path) -> Result<()> {
    use futures_util::TryStreamExt;

    let file = tokio::fs::File::open(tar)
        .await
        .with_context(|| format!("failed to open the exported image at {}", tar.display()))?;
    let body = bollard::body_try_stream(tokio_util::io::ReaderStream::new(file));

    docker
        .import_image(
            bollard::query_parameters::ImportImageOptionsBuilder::default().build(),
            body,
            None,
        )
        .try_collect::<Vec<_>>()
        .await
        .context("failed to load the built image into docker")?;
    Ok(())
}

/// Sets or clears the process-global `SSH_AUTH_SOCK`. Only ever called with
/// `SSH_AUTH_SOCK_ENV` held — see that lock's doc for why this has to be
/// process-global at all. `set_var` is `unsafe` in edition 2024 because it
/// races with any *other* thread reading the environment; the lock makes
/// fghjd's own accesses exclusive, and the read that matters (bollard's
/// `check_agent`) happens inside the build this brackets.
fn set_ssh_auth_sock(value: Option<&str>) {
    unsafe {
        match value {
            Some(value) => std::env::set_var("SSH_AUTH_SOCK", value),
            None => std::env::remove_var("SSH_AUTH_SOCK"),
        }
    }
}

/// Splits an OCI platform string (`linux/arm64`, `linux/arm/v7`) into
/// BuildKit's structured form.
///
/// Anything that isn't at least `os/arch` is an error rather than a silent
/// fallback to the daemon's own platform. It used to be a fallback, which was
/// defensible while the classic builder still handled most builds and passed
/// `#Build.platform` through to Docker to reject itself; now that every build
/// comes through here, swallowing it would mean `platform: arm64` (a common
/// mistake — the arch alone, no os) quietly building for the host instead.
fn parse_platform(platform: &str) -> Result<bollard::grpc::build::ImageBuildPlatform> {
    let mut parts = platform.split('/');
    let os = parts.next().unwrap_or_default();
    let architecture = parts.next().unwrap_or_default();
    if os.is_empty() || architecture.is_empty() {
        bail!(
            "`platform: {platform}` is not a valid OCI platform:              it needs at least os/arch, e.g. linux/arm64"
        );
    }
    Ok(bollard::grpc::build::ImageBuildPlatform {
        architecture: architecture.to_string(),
        os: os.to_string(),
        variant: parts.next().map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a context laid out like the repos this was diagnosed on and reads
    /// the resulting tar back, which is the only way to see what was actually
    /// uploaded.
    async fn tar_entries(dir: &Path, dockerfile: &str) -> Vec<String> {
        let bytes = tar_build_context(dir, dockerfile, None, false)
            .await
            .unwrap();
        let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
        archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .filter(|p| !p.is_empty() && p != ".")
            .collect()
    }

    /// The bug: `.git` reaching the builder makes Go stamp VCS info, `git`
    /// inside the container exits 128, and the build fails with an error that
    /// names the compile step and nothing about the context.
    #[tokio::test]
    async fn a_dockerignore_keeps_git_and_vendor_trees_out_of_the_context() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join(".dockerignore"),
            ".git\nnode_modules\n.env\n*.test\n",
        )
        .unwrap();
        std::fs::write(root.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(root.join("main.go"), "package main\n").unwrap();
        std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(root.join("handler.test"), "x").unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(root.join("node_modules").join("left-pad")).unwrap();
        std::fs::write(root.join("node_modules").join("left-pad").join("i.js"), "x").unwrap();

        let entries = tar_entries(root, "Dockerfile").await;

        assert!(entries.contains(&String::from("main.go")));
        for excluded in [".git", ".git/HEAD", "node_modules", ".env", "handler.test"] {
            assert!(
                !entries.iter().any(|e| e == excluded),
                "{excluded} should not have been uploaded, got: {entries:?}"
            );
        }
        assert!(
            !entries.iter().any(|e| e.starts_with("node_modules")),
            "nothing under node_modules should be uploaded, got: {entries:?}"
        );
    }

    /// A git build context: the inline Dockerfile rides in the tar, so the
    /// clone stays untouched, and the clone's `.git` stays out even with no
    /// `.dockerignore` — and even with one that tries to bring it back.
    #[tokio::test]
    async fn an_inline_dockerfile_is_sent_and_a_clones_git_dir_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("main.go"), "package main\n").unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let bytes = tar_build_context(root, INLINE_DOCKERFILE, Some("FROM scratch\n"), true)
            .await
            .unwrap();
        let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
        let mut entries = Vec::new();
        let mut inline = String::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            if path == INLINE_DOCKERFILE {
                std::io::Read::read_to_string(&mut entry, &mut inline).unwrap();
            }
            entries.push(path);
        }
        assert_eq!(inline, "FROM scratch\n");
        assert!(entries.contains(&String::from("main.go")));
        assert!(
            !entries
                .iter()
                .any(|e| e.starts_with(".git/") || e == ".git"),
            "the clone's .git should not have been uploaded, got: {entries:?}"
        );
        assert!(!root.join(INLINE_DOCKERFILE).exists());

        std::fs::write(root.join(".dockerignore"), "*.md\n!.git\n").unwrap();
        let bytes = tar_build_context(root, "Dockerfile", None, true)
            .await
            .unwrap();
        let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
        assert!(archive.entries().unwrap().all(|e| {
            let path = e.unwrap().path().unwrap().to_string_lossy().into_owned();
            !path.starts_with(".git/") && path != ".git"
        }));
    }

    /// Real `.dockerignore` files exclude the Dockerfile, because `COPY . .`
    /// would otherwise bake it into the image. Honouring that literally would
    /// leave BuildKit nothing to build — so filtering the context must not
    /// become a new way to break one.
    #[tokio::test]
    async fn the_dockerfile_is_sent_even_when_the_ignore_file_excludes_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join(".dockerignore"),
            "Dockerfile\n.dockerignore\ndocker-compose*.yml\n",
        )
        .unwrap();
        std::fs::write(root.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(root.join("docker-compose.yml"), "services: {}\n").unwrap();
        std::fs::write(root.join("app.js"), "console.log(1)\n").unwrap();

        let entries = tar_entries(root, "Dockerfile").await;

        assert!(
            entries.contains(&String::from("Dockerfile")),
            "got: {entries:?}"
        );
        assert!(entries.contains(&String::from("app.js")));
        assert!(!entries.iter().any(|e| e == "docker-compose.yml"));
    }

    /// A custom `-f` name has to be the one that survives, not the literal
    /// string "Dockerfile".
    #[tokio::test]
    async fn a_custom_dockerfile_name_is_the_one_that_survives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".dockerignore"), "*.dockerfile\n").unwrap();
        std::fs::write(root.join("prod.dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(root.join("dev.dockerfile"), "FROM scratch\n").unwrap();

        let entries = tar_entries(root, "prod.dockerfile").await;

        assert!(
            entries.contains(&String::from("prod.dockerfile")),
            "got: {entries:?}"
        );
        assert!(!entries.iter().any(|e| e == "dev.dockerfile"));
    }

    /// An exception has to be able to reach inside an excluded directory, which
    /// is the one case where the directory must still be walked.
    #[tokio::test]
    async fn an_exception_reaches_inside_an_excluded_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".dockerignore"), "assets\n!assets/keep.txt\n").unwrap();
        std::fs::create_dir(root.join("assets")).unwrap();
        std::fs::write(root.join("assets").join("keep.txt"), "keep").unwrap();
        std::fs::write(root.join("assets").join("drop.bin"), "drop").unwrap();

        let entries = tar_entries(root, "Dockerfile").await;

        assert!(
            entries.contains(&String::from("assets/keep.txt")),
            "got: {entries:?}"
        );
        assert!(!entries.iter().any(|e| e == "assets/drop.bin"));
    }

    /// No `.dockerignore` must behave exactly as before, dotfiles included —
    /// this path is what every existing build already takes.
    #[tokio::test]
    async fn no_dockerignore_uploads_everything_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(root.join(".hidden"), "x").unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();

        let entries = tar_entries(root, "Dockerfile").await;

        for expected in ["Dockerfile", ".hidden", "src/main.rs"] {
            assert!(
                entries.iter().any(|e| e == expected),
                "{expected} should still be uploaded, got: {entries:?}"
            );
        }
    }

    /// The exact message BuildKit returned for a failed `go build`, taken
    /// from a real failure — the one that reached the events pane as three
    /// copies of itself.
    #[test]
    fn a_failed_build_step_splits_into_a_command_and_an_exit_code() {
        let (step, exit) = split_step_failure(
            "process \"/bin/sh -c CGO_ENABLED=0 GOOS=linux go build -o /out/aikifactory ./cmd/aikifactory\" did not complete successfully: exit code: 1",
        );
        assert_eq!(
            step,
            Some(
                "/bin/sh -c CGO_ENABLED=0 GOOS=linux go build -o /out/aikifactory ./cmd/aikifactory"
            )
        );
        assert_eq!(exit, Some("1"));
    }

    /// Anything that isn't that shape is passed through whole rather than
    /// being forced into fields it doesn't have. A missing base image is
    /// already a sentence; splitting it would lose it.
    #[test]
    fn a_message_that_is_not_a_step_failure_is_left_alone() {
        assert_eq!(
            split_step_failure("failed to solve: nonexistent: not found"),
            (None, None)
        );
        assert_eq!(split_step_failure(""), (None, None));
        // The prefix without the marker: truncated or a future wording.
        assert_eq!(
            split_step_failure("process \"sh -c x\" exploded"),
            (None, None)
        );
    }

    /// A command containing the marker's own words must not split early —
    /// `find` is the first match, so a step that echoes the phrase would
    /// otherwise cut the command in half.
    #[test]
    fn the_split_takes_the_quote_before_the_marker() {
        let (step, exit) = split_step_failure(
            "process \"/bin/sh -c echo did not complete successfully\" did not complete successfully: exit code: 2",
        );
        assert_eq!(step, Some("/bin/sh -c echo did not complete successfully"));
        assert_eq!(exit, Some("2"));
    }
}

#[cfg(test)]
mod build_tests {
    use super::*;
    use std::process::Command;

    /// A throwaway build context plus the image tag it produces, both cleaned
    /// up on drop. `docker rmi` rather than leaving images behind matters
    /// here: every test in this module builds a *different* image under a
    /// unique tag, so without it a test run leaks one dangling image apiece.
    struct BuildFixture {
        dir: tempfile::TempDir,
        tag: String,
    }

    impl BuildFixture {
        fn new(label: &str, dockerfile: &str) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(dir.path().join("Dockerfile"), dockerfile).expect("write Dockerfile");
            let tag = format!("fghj-build-test/{label}:{}", crate::docker::rand_suffix());
            Self { dir, tag }
        }

        fn write(&self, name: &str, contents: &str) -> std::path::PathBuf {
            let path = self.dir.path().join(name);
            std::fs::write(&path, contents).expect("write fixture file");
            path
        }

        /// Runs the built image and returns its combined output, which is how
        /// every test here checks what actually landed *inside* the image —
        /// the only evidence that a build input reached the daemon rather
        /// than being accepted and dropped.
        fn run(&self, cmd: &[&str]) -> String {
            let mut args = vec!["run", "--rm", &self.tag];
            args.extend_from_slice(cmd);
            let out = Command::new("docker")
                .args(&args)
                .output()
                .expect("docker run of the built image");
            assert!(
                out.status.success(),
                "docker run failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
    }

    impl Drop for BuildFixture {
        fn drop(&mut self) {
            let _ = Command::new("docker")
                .args(["rmi", "-f", &self.tag])
                .status();
        }
    }

    fn docker() -> Docker {
        Docker::connect_with_local_defaults().expect("connect to docker")
    }

    #[test]
    fn a_platform_string_splits_into_buildkits_structured_form() {
        let two = parse_platform("linux/arm64").expect("os/arch parses");
        assert_eq!(two.os, "linux");
        assert_eq!(two.architecture, "arm64");
        assert_eq!(two.variant, None);

        let three = parse_platform("linux/arm/v7").expect("os/arch/variant parses");
        assert_eq!(three.variant.as_deref(), Some("v7"));

        // Anything the daemon couldn't have meant fails the build rather
        // than quietly building for the host — `platform: arm64`, the arch
        // with no os, is the mistake this catches.
        for bad in ["linux", "linux/", "", "arm64"] {
            let err = parse_platform(bad).expect_err("not a platform");
            assert!(err.to_string().contains("os/arch"), "got: {err}");
        }
    }

    /// The regression test for the bug E3 turned up: `build.args` was parsed,
    /// carried into the graph, and then silently dropped on the way to the
    /// daemon. `target` was never plumbed at all. Both are checked at once
    /// because a build that honours `target` but not `args` and one that
    /// honours neither produce *different* wrong answers here.
    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn build_args_and_target_both_reach_the_daemon() {
        let fixture = BuildFixture::new(
            "args-target",
            "FROM busybox AS wanted\n\
             ARG GREETING=unset\n\
             RUN echo \"$GREETING\" > /greeting\n\
             FROM busybox AS unwanted\n\
             RUN echo wrong-stage > /greeting\n",
        );
        let mut args = BTreeMap::new();
        args.insert(String::from("GREETING"), String::from("hello-from-args"));

        build_image(
            &docker(),
            &BuildOpts {
                context_dir: fixture.dir.path(),
                dockerfile: "Dockerfile",
                dockerfile_inline: None,
                skip_git_dir: false,
                tag: &fixture.tag,
                platform: None,
                args: &args,
                target: Some("wanted"),
                secrets: &[],
                ssh_auth_sock: None,
                builder: None,
            },
        )
        .await
        .expect("classic build with args and target");

        assert_eq!(fixture.run(&["cat", "/greeting"]), "hello-from-args");
    }

    /// A build secret is mounted for the length of one `RUN` and must not be
    /// in the finished image — so this asserts both halves: the bytes are
    /// readable during the build, and nothing survives into the image. The
    /// mount point is gone entirely afterwards, not merely emptied.
    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_file_secret_is_readable_during_the_build_and_absent_after_it() {
        let fixture = BuildFixture::new(
            "secret",
            "FROM busybox\n\
             RUN --mount=type=secret,id=token cp /run/secrets/token /copied-during-build\n",
        );
        let secret_path = fixture.write("token.txt", "s3cr3t-value");

        build_image(
            &docker(),
            &BuildOpts {
                context_dir: fixture.dir.path(),
                dockerfile: "Dockerfile",
                dockerfile_inline: None,
                skip_git_dir: false,
                tag: &fixture.tag,
                platform: None,
                args: &BTreeMap::new(),
                target: None,
                secrets: &[(String::from("token"), secret_path)],
                ssh_auth_sock: None,
                builder: None,
            },
        )
        .await
        .expect("buildkit build with a file secret");

        assert_eq!(
            fixture.run(&["cat", "/copied-during-build"]),
            "s3cr3t-value"
        );
        assert_eq!(
            fixture.run(&[
                "sh",
                "-c",
                "test -e /run/secrets && echo leaked || echo clean"
            ]),
            "clean"
        );
    }

    /// The BuildKit path has to surface a failing `RUN` as an error, not
    /// swallow it — `Build::docker_build` collapses a whole solve into one
    /// `Result`, so this is the only thing standing between a broken
    /// Dockerfile and a "successful" build of a nonexistent image.
    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_failing_run_under_buildkit_is_reported_as_an_error() {
        let fixture = BuildFixture::new(
            "failing",
            "FROM busybox\n\
             RUN --mount=type=secret,id=token exit 7\n",
        );
        let secret_path = fixture.write("token.txt", "unused");

        let err = build_image(
            &docker(),
            &BuildOpts {
                context_dir: fixture.dir.path(),
                dockerfile: "Dockerfile",
                dockerfile_inline: None,
                skip_git_dir: false,
                tag: &fixture.tag,
                platform: None,
                args: &BTreeMap::new(),
                target: None,
                secrets: &[(String::from("token"), secret_path)],
                ssh_auth_sock: None,
                builder: None,
            },
        )
        .await
        .expect_err("a RUN that exits 7 must fail the build");
        // Asserts on the *useful* parts, not the prose around them: which
        // Dockerfile line died and what it exited with. Those are what a person
        // reads the error for, and they are the pieces `describe_solve_failure`
        // has to dig out of BuildKit's single collapsed `Result`. An earlier
        // version of this test matched the opening words instead, and went red
        // when the wording improved while the diagnosis got better.
        let report = format!("{err:#}");
        for expected in [
            fixture.tag.as_str(),
            "failing step: /bin/sh -c exit 7",
            "exit code: 7",
        ] {
            assert!(
                report.contains(expected),
                "error should mention {expected:?}, got: {report}"
            );
        }
    }

    // ---- building on the host's own buildx builder ----

    /// The builder `docker build` would use on this machine.
    ///
    /// Resolved from `$HOME` rather than a `WorkspaceOwner`, because a test
    /// binary runs as the user — that is the one place where the process's own
    /// home *is* the right one to read. Panics rather than silently passing if
    /// there is no container-backed builder selected: a test named for the host
    /// builder that quietly tested the embedded one would be worse than no
    /// test.
    fn host_builder() -> crate::buildx::HostBuilder {
        let home = std::env::var("HOME").expect("HOME");
        crate::buildx::default_builder(std::path::Path::new(&home)).expect(
            "this test needs a container-backed buildx builder selected \
             (`docker buildx ls` — the one with the `*`)",
        )
    }

    fn container_id(name: &str) -> Option<String> {
        let out = Command::new("docker")
            .args(["inspect", "-f", "{{.Id}}{{.State.Running}}", name])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Containers bollard creates when it is given no fixed name. Any of these
    /// appearing means the build ran on a throwaway BuildKit — correct, but
    /// with a cold cache and a leaked container and volume per build.
    fn throwaway_buildkits() -> Vec<String> {
        let out = Command::new("docker")
            .args(["ps", "-a", "--format", "{{.Names}}"])
            .output()
            .expect("docker ps");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|n| n.starts_with("bollard_buildkit_"))
            .map(str::to_owned)
            .collect()
    }

    /// The BuildKit version actually running inside a builder container.
    ///
    /// Asked of the running process rather than inferred from the image
    /// reference, because that is the property under test and the image
    /// reference is a bad proxy for it: buildx's default tag
    /// (`moby/buildkit:buildx-stable-1`) moves, so two containers can share a
    /// tag and run engines months apart. An earlier version of this test
    /// compared the tags and passed while fghj ran v0.33.1 against buildx's
    /// v0.32.2 — exactly the divergence the feature exists to remove.
    fn buildkit_version(container: &str) -> String {
        let out = Command::new("docker")
            .args(["exec", container, "buildkitd", "--version"])
            .output()
            .expect("docker exec buildkitd --version");
        assert!(
            out.status.success(),
            "could not read buildkitd version from {container}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        // "buildkitd github.com/moby/buildkit v0.32.2 991535e…" -> "v0.32.2"
        text.split_whitespace()
            .find(|w| w.starts_with('v') && w[1..].starts_with(|c: char| c.is_ascii_digit()))
            .unwrap_or_else(|| panic!("no version in buildkitd output: {text}"))
            .to_string()
    }

    /// The point of the whole exercise: a build routed at the host's buildx
    /// builder still ends up as a runnable image in *dockerd's* store.
    ///
    /// That is not automatic. A container-backed BuildKit cannot write to
    /// dockerd's image store, so this path exports a tarball and loads it —
    /// three steps where the `Moby` driver has one. Running the image is the
    /// only evidence that all three worked; `inspect_image` would also pass on
    /// a manifest with no layers behind it.
    #[tokio::test]
    #[ignore = "needs a Docker daemon and a buildx builder: see concepts/release-and-delivery.md"]
    async fn a_build_on_the_host_builder_lands_in_dockers_image_store() {
        let builder = host_builder();
        let fixture = BuildFixture::new(
            "host-builder",
            "FROM busybox\n\
             RUN echo built-on-host-builder > /where\n",
        );

        let report = build_image(
            &docker(),
            &BuildOpts {
                context_dir: fixture.dir.path(),
                dockerfile: "Dockerfile",
                dockerfile_inline: None,
                skip_git_dir: false,
                tag: &fixture.tag,
                platform: None,
                args: &BTreeMap::new(),
                target: None,
                secrets: &[],
                ssh_auth_sock: None,
                builder: Some(&builder),
            },
        )
        .await
        .expect("build on the host builder");

        assert_eq!(fixture.run(&["cat", "/where"]), "built-on-host-builder");
        // The report has to name the builder, or the events pane cannot tell
        // the user which engine ran their build — the thing that made this
        // divergence invisible in the first place.
        assert_eq!(report.builder.as_deref(), Some(builder.builder.as_str()));
    }

    /// The three properties that make this worth doing at all, checked
    /// together because they only mean something jointly: fghj's builder is
    /// *reused* (warm cache), it runs the *same image* as buildx's (version
    /// parity, the actual bug), and buildx's own container is *untouched* (fghj
    /// does not disturb the user's CLI).
    #[tokio::test]
    #[ignore = "needs a Docker daemon and a buildx builder: see concepts/release-and-delivery.md"]
    async fn fghjs_builder_is_reused_matches_buildxs_image_and_leaves_buildx_alone() {
        let builder = host_builder();
        let buildx_before = container_id(&builder.container);
        assert!(
            buildx_before.is_some(),
            "expected buildx's BuildKit container {} to exist; \
             run a `docker build` once to bootstrap it",
            builder.container
        );
        let buildx_version = buildkit_version(&builder.container);
        let throwaways_before = throwaway_buildkits();

        // Twice: the first call may create fghj's builder, so only the second
        // can show that an existing one is reused rather than replaced.
        let mut ids = Vec::new();
        for label in ["host-reuse-1", "host-reuse-2"] {
            let fixture = BuildFixture::new(label, "FROM busybox\nRUN true\n");
            build_image(
                &docker(),
                &BuildOpts {
                    context_dir: fixture.dir.path(),
                    dockerfile: "Dockerfile",
                    dockerfile_inline: None,
                    skip_git_dir: false,
                    tag: &fixture.tag,
                    platform: None,
                    args: &BTreeMap::new(),
                    target: None,
                    secrets: &[],
                    ssh_auth_sock: None,
                    builder: Some(&builder),
                },
            )
            .await
            .expect("build on fghj's builder");
            ids.push(container_id(FGHJ_BUILDKIT_CONTAINER));
        }

        assert!(
            ids[0].is_some(),
            "fghj's BuildKit container was not created"
        );
        assert_eq!(
            ids[0], ids[1],
            "fghj's BuildKit container was replaced between builds, so every \
             build starts with a cold cache"
        );
        // The whole point: byte-identical BuildKit to the one the CLI uses, so
        // `fghj` and `docker build` cannot disagree about a Dockerfile.
        assert_eq!(
            buildkit_version(FGHJ_BUILDKIT_CONTAINER),
            buildx_version,
            "fghj's builder runs a different BuildKit version from buildx's, \
             so the two can still disagree about a Dockerfile"
        );
        // And buildx's own builder is left exactly as it was.
        assert_eq!(
            container_id(&builder.container),
            buildx_before,
            "buildx's BuildKit container was replaced or stopped"
        );
        assert_eq!(
            throwaway_buildkits(),
            throwaways_before,
            "a throwaway BuildKit container was created, so the build had a \
             cold cache and leaked a container"
        );
    }

    /// The export path reassembles the frontend options independently of the
    /// `Build` path, so inputs that are known to have been dropped before
    /// (`concepts/AUDIT.md` E3) are re-checked on this route rather than
    /// assumed to carry over.
    #[tokio::test]
    #[ignore = "needs a Docker daemon and a buildx builder: see concepts/release-and-delivery.md"]
    async fn build_args_and_target_survive_the_host_builder_route() {
        let builder = host_builder();
        let fixture = BuildFixture::new(
            "host-args-target",
            "FROM busybox AS wanted\n\
             ARG GREETING=unset\n\
             RUN echo \"$GREETING\" > /greeting\n\
             FROM busybox AS unwanted\n\
             RUN echo wrong-stage > /greeting\n",
        );
        let mut args = BTreeMap::new();
        args.insert(String::from("GREETING"), String::from("hello-host-builder"));

        build_image(
            &docker(),
            &BuildOpts {
                context_dir: fixture.dir.path(),
                dockerfile: "Dockerfile",
                dockerfile_inline: None,
                skip_git_dir: false,
                tag: &fixture.tag,
                platform: None,
                args: &args,
                target: Some("wanted"),
                secrets: &[],
                ssh_auth_sock: None,
                builder: Some(&builder),
            },
        )
        .await
        .expect("build on the host builder");

        assert_eq!(fixture.run(&["cat", "/greeting"]), "hello-host-builder");
    }
}
