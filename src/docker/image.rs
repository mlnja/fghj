use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bollard::Docker;
use bollard::grpc::error::GrpcError;
use bollard::query_parameters::CreateImageOptionsBuilder;
use futures_util::StreamExt;

pub struct BuildOpts<'a> {
    pub context_dir: &'a Path,
    pub dockerfile: &'a str,
    pub dockerfile_inline: Option<&'a str>,
    /// Leave out `.git`, whatever `.dockerignore` says: a git build context's
    /// history is fghj's clone, not the build's.
    pub skip_git_dir: bool,
    pub tag: &'a str,
    pub platform: Option<&'a str>,
    pub args: &'a BTreeMap<String, String>,
    pub target: Option<&'a str>,
    pub secrets: &'a [(String, PathBuf)],
    /// A live agent socket to lend the build; `fghjd` is root and has none.
    pub ssh_auth_sock: Option<&'a str>,
    /// `None` is dockerd's embedded BuildKit, a different engine from the one
    /// `docker build` uses when buildx has a container builder selected.
    pub builder: Option<&'a crate::buildx::HostBuilder>,
}

/// bollard reads `SSH_AUTH_SOCK` from the process environment, so it has to be
/// set around the build; this lock keeps two ssh builds from overlapping.
static SSH_AUTH_SOCK_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Anonymous pull; only used for fghj's own public sidecar image.
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

#[derive(Debug)]
pub struct BuildReport {
    pub context_bytes: u64,
    pub builder: Option<String>,
}

/// BuildKit's raw error repeats the message three times. This keeps the
/// failing step and exit code once and says where the step's output is. The
/// code is dropped: BuildKit reports every failed step as `Unknown`.
fn describe_solve_failure(opts: &BuildOpts<'_>, err: GrpcError) -> anyhow::Error {
    let GrpcError::TonicStatus { err: status } = &err else {
        return anyhow::Error::from(err).context(format!("buildkit build of {} failed", opts.tag));
    };

    let message = status.message().trim().to_string();
    let (step, exit) = split_step_failure(&message);

    let mut out = format!("build of {} failed", opts.tag);
    match (step, exit) {
        (Some(step), Some(exit)) => {
            out.push_str(&format!("\nfailing step: {step}\nexit code: {exit}"));
        }
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

/// Splits `process "..." did not complete successfully: exit code: N` into
/// command and code; `(None, None)` for anything else.
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

/// Builds `opts.tag`, always through BuildKit. See `concepts/build-inputs.md`.
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

/// Reserved: a file of this name in the context is replaced.
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
            // Last, so no `!` pattern can bring `.git` back.
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

/// Walks the context, leaving out what `.dockerignore` excludes — except the
/// Dockerfile and `.dockerignore` themselves, which are always sent, as the
/// Docker CLI does.
fn append_filtered(
    builder: &mut tar::Builder<Vec<u8>>,
    root: &Path,
    ignore: &crate::dockerignore::Dockerignore,
    dockerfile: &str,
) -> Result<()> {
    let always = [dockerfile.trim_start_matches("./"), ".dockerignore"];
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
            // `metadata` follows symlinks, like `append_dir_all`; a broken link is skipped.
            let Ok(meta) = std::fs::metadata(&abs) else {
                continue;
            };
            let is_dir = meta.is_dir();
            if !always.contains(&rel.as_str()) && ignore.excludes(&rel) {
                // An excluded directory is walked only if a `!` pattern could re-include
                // something inside it.
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
        // `SecretSource::Env` is unreachable until `concepts/AUDIT.md` E2 closes.
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
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(grpc)) => Err(describe_solve_failure(opts, grpc)),
        Err(e) => Err(e.context(format!("buildkit build of {tag} failed"))),
    }
}

/// bollard's solve futures are `!Send`, so they run on a current-thread runtime
/// on a thread of their own and only the result crosses back.
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

/// Fixed name, so its state volume (and cache) persists. Not buildx's own
/// container: bollard refuses to adopt one it didn't create.
const FGHJ_BUILDKIT_CONTAINER: &str = "fghj_buildkit";

/// Solves on a BuildKit container running the same image as the host's buildx
/// builder, so fghj and `docker build` agree, then loads the result into
/// dockerd (a container BuildKit can't write to its store).
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

    // Not `/tmp`, which is small on macOS; an image tarball is image-sized.
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
            // A fixed name keeps the cache across builds.
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

/// buildx's BuildKit image, pinned by repo digest: its default tag moves, so a
/// tag match could still be a different version. Falls back to the tag, then
/// bollard's default.
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

/// Removes fghj's BuildKit container if its image no longer matches buildx's;
/// the state volume (cache) is kept. Best effort.
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
                    .v(false)
                    .build(),
            ),
        )
        .await;
}

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

/// Only called with `SSH_AUTH_SOCK_ENV` held.
fn set_ssh_auth_sock(value: Option<&str>) {
    unsafe {
        match value {
            Some(value) => std::env::set_var("SSH_AUTH_SOCK", value),
            None => std::env::remove_var("SSH_AUTH_SOCK"),
        }
    }
}

/// Anything short of `os/arch` is an error, so `platform: arm64` doesn't
/// silently build for the host.
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

    #[test]
    fn a_message_that_is_not_a_step_failure_is_left_alone() {
        assert_eq!(
            split_step_failure("failed to solve: nonexistent: not found"),
            (None, None)
        );
        assert_eq!(split_step_failure(""), (None, None));
        assert_eq!(
            split_step_failure("process \"sh -c x\" exploded"),
            (None, None)
        );
    }

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

        for bad in ["linux", "linux/", "", "arm64"] {
            let err = parse_platform(bad).expect_err("not a platform");
            assert!(err.to_string().contains("os/arch"), "got: {err}");
        }
    }

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

    /// Panics with no container builder selected, rather than silently testing
    /// the embedded one.
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

    /// Asks the running process: the tag is a bad proxy for the version.
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
        text.split_whitespace()
            .find(|w| w.starts_with('v') && w[1..].starts_with(|c: char| c.is_ascii_digit()))
            .unwrap_or_else(|| panic!("no version in buildkitd output: {text}"))
            .to_string()
    }

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
        assert_eq!(report.builder.as_deref(), Some(builder.builder.as_str()));
    }

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
        assert_eq!(
            buildkit_version(FGHJ_BUILDKIT_CONTAINER),
            buildx_version,
            "fghj's builder runs a different BuildKit version from buildx's, \
             so the two can still disagree about a Dockerfile"
        );
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
