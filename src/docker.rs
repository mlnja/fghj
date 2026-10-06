use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::{Context, Result, bail};
use bollard::Docker;
use bollard::exec::{CreateExecOptions, ResizeExecOptions, StartExecOptions, StartExecResults};
use bollard::grpc::error::GrpcError;
use bollard::models::{
    ContainerCreateBody, EndpointSettings, HealthConfig, HostConfig, NetworkCreateRequest,
    NetworkingConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum, VolumeCreateRequest,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, InspectContainerOptionsBuilder,
    ListVolumesOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    RemoveVolumeOptionsBuilder,
};
use futures_util::StreamExt;
use futures_util::stream::Stream;
use tokio::io::AsyncWrite;

use crate::resolver::Healthcheck;

/// Maps `#RunOptions.restart`'s CUE-side spelling to bollard's enum — an
/// unrecognized value (shouldn't happen once `fghj validate` has run, but
/// `resolver/` parses YAML independently of CUE, see its module doc) falls
/// back to `"no"` rather than erroring, matching the CUE default.
fn restart_policy_name(restart: &str) -> RestartPolicyNameEnum {
    match restart {
        "always" => RestartPolicyNameEnum::ALWAYS,
        "on-failure" => RestartPolicyNameEnum::ON_FAILURE,
        "unless-stopped" => RestartPolicyNameEnum::UNLESS_STOPPED,
        _ => RestartPolicyNameEnum::NO,
    }
}

/// `project` mirrors Docker Compose's own network labels
/// (`com.docker.compose.network`/`.project`) — without them, Docker
/// Desktop/OrbStack can group this network's containers under `project` (by
/// their own container-level labels, set in `run_container` below) but can't
/// tell this network belongs to that same project, since name alone isn't a
/// recognized grouping signal. That breaks "delete group" in those UIs: it
/// can find and remove the containers, but can't identify the network as
/// part of the same atomic delete, so the whole group action fails instead
/// of leaving an orphaned network behind.
pub async fn ensure_network(docker: &Docker, name: &str, project: &str) -> Result<()> {
    let mut labels = HashMap::new();
    labels.insert(
        "com.docker.compose.network".to_string(),
        "default".to_string(),
    );
    labels.insert(
        "com.docker.compose.project".to_string(),
        project.to_string(),
    );
    let result = docker
        .create_network(NetworkCreateRequest {
            name: name.to_string(),
            labels: Some(labels),
            ..Default::default()
        })
        .await;
    match result {
        Ok(_) => Ok(()),
        // a network by this name already existing is fine — that's the point
        // of "ensure"; any other failure is real.
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 409, ..
        }) => Ok(()),
        Err(e) => Err(e).context("docker create_network failed"),
    }
}

pub async fn remove_network(docker: &Docker, name: &str) {
    let _ = docker.remove_network(name).await;
}

/// A named volume is otherwise Docker-implicit — the daemon auto-creates
/// one the first time a container's `Binds` references a name that
/// doesn't exist yet, but never labels it. Calling this explicitly first
/// (mirrors `ensure_network` above) attaches the same
/// `com.docker.compose.project` bookkeeping label containers/networks
/// already get, plus fghj's own `fghj.scope`/`fghj.run` — which is what
/// lets `remove_run_scoped_volumes` below find and clean up a `scope:
/// "run"` preview/named run's volumes once that run stops, closing the "no
/// `docker compose down -v` equivalent" gap called out in the `#Volume`
/// docs. Unlike `create_network`, creating a volume that already exists
/// isn't an error — the API just returns the existing one — so there's no
/// "already exists" case to special-case.
pub async fn ensure_volume(
    docker: &Docker,
    name: &str,
    project: &str,
    scope: &str,
    run_id: &str,
) -> Result<()> {
    let mut labels = HashMap::new();
    labels.insert(
        "com.docker.compose.project".to_string(),
        project.to_string(),
    );
    labels.insert("fghj.scope".to_string(), scope.to_string());
    labels.insert("fghj.run".to_string(), run_id.to_string());
    docker
        .create_volume(VolumeCreateRequest {
            name: Some(name.to_string()),
            labels: Some(labels),
            ..Default::default()
        })
        .await
        .context("docker create_volume failed")?;
    Ok(())
}

/// Best-effort removal of every `scope: "run"` named volume `ensure_volume`
/// labeled for `run_id` — the other half of closing the "no `docker
/// compose down -v` equivalent" gap. Docker ANDs multiple `label=`
/// filter values together (unlike most other filter types, which OR), so
/// this only matches a volume carrying *both* labels — never a `scope:
/// "stable"` volume, even one created under the same run, since that
/// scope's entire point is to outlive any one run.
///
/// Deliberately never called for the *default* run (see
/// `RunRegistry::stop`'s own call site) — a `"run"`-scoped volume there
/// gets the exact same derived name on every start (`derive_domain`, which
/// `derive_volume_name` reuses, only folds the run id in for a *named*
/// run), so deleting it on stop would silently wipe data the next
/// default-run start expects to still be there.
pub async fn remove_run_scoped_volumes(docker: &Docker, run_id: &str) {
    let mut filters: HashMap<&str, Vec<String>> = HashMap::new();
    filters.insert(
        "label",
        vec![format!("fghj.run={run_id}"), "fghj.scope=run".to_string()],
    );
    let Ok(listed) = docker
        .list_volumes(Some(
            ListVolumesOptionsBuilder::new().filters(&filters).build(),
        ))
        .await
    else {
        return;
    };
    for volume in listed.volumes.unwrap_or_default() {
        let _ = docker
            .remove_volume(
                &volume.name,
                Some(RemoveVolumeOptionsBuilder::new().force(true).build()),
            )
            .await;
    }
}

/// Lists the name of every Docker volume `ensure_volume` has ever labeled
/// for `run_id`, regardless of `scope` — the read-only counterpart to
/// `remove_run_scoped_volumes` (which deliberately restricts itself to
/// `scope: "run"` only). Used by `effects::docker::observe` to discover
/// what volumes actually exist for a run, since `state::RunState::volumes`
/// starts out empty for every run (`effects::docker::converge::mirror_run`
/// never had anything to populate it from) and there is no other source of
/// truth for volume identity yet.
pub async fn list_run_volumes(docker: &Docker, run_id: &str) -> Result<Vec<String>> {
    let mut filters: HashMap<&str, Vec<String>> = HashMap::new();
    filters.insert("label", vec![format!("fghj.run={run_id}")]);
    let listed = docker
        .list_volumes(Some(
            ListVolumesOptionsBuilder::new().filters(&filters).build(),
        ))
        .await
        .context("docker list_volumes failed")?;
    Ok(listed
        .volumes
        .unwrap_or_default()
        .into_iter()
        .map(|v| v.name)
        .collect())
}

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
/// A `Docker` handle for tests that never actually talk to Docker — the ones
/// about bookkeeping that happens to live behind a type owning a client.
///
/// `connect_with_local_defaults` cannot serve this purpose: bollard resolves
/// the unix socket at *construction* and returns `SocketNotFoundError` when it
/// isn't there, so a test that sends no request still fails on any machine
/// without Docker — which is every CI runner this project can use, for the
/// reasons `concepts/release-and-delivery.md` sets out. The http transport
/// does no such check. Port 1 is deliberate: nothing listens there, so a
/// request that shouldn't happen fails loudly instead of quietly reaching a
/// real daemon.
#[cfg(test)]
pub(crate) fn undialled_client() -> Docker {
    Docker::connect_with_http("http://127.0.0.1:1", 1, bollard::API_DEFAULT_VERSION)
        .expect("constructing an http transport cannot fail")
}

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
    let tar_bytes = tar_build_context(opts.context_dir).await?;
    let context_bytes = tar_bytes.len() as u64;
    build_image_buildkit(docker, opts, tar_bytes).await?;
    Ok(BuildReport { context_bytes })
}

async fn tar_build_context(context_dir: &Path) -> Result<Vec<u8>> {
    let context_dir = context_dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_dir_all("", &context_dir)
            .with_context(|| format!("failed to tar build context {}", context_dir.display()))?;
        builder
            .into_inner()
            .context("failed to finalize build context tar")
    })
    .await
    .context("tar task panicked")?
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

    let result = match opts.ssh_auth_sock {
        // The guard is scoped to the match arm so the process-global variable
        // is restored — and the lock released — the moment this one build
        // finishes, not at the end of the function.
        Some(sock) => {
            let _guard = SSH_AUTH_SOCK_ENV.lock().await;
            let previous = std::env::var_os("SSH_AUTH_SOCK");
            set_ssh_auth_sock(Some(sock));
            let result = solve_on_dedicated_thread(docker, tag, frontend, load).await;
            set_ssh_auth_sock(previous.as_ref().and_then(|v| v.to_str()));
            result
        }
        None => solve_on_dedicated_thread(docker, tag, frontend, load).await,
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

/// Runs one BuildKit solve on a thread of its own.
///
/// bollard's `Build::docker_build` future is `!Send` — its driver tear-down
/// handler is a bare `Box<dyn Future>` — so awaiting it inline would make
/// every caller up to and including `tokio::spawn` in `daemon::reconcile`
/// `!Send` too. Rather than restructure the daemon around one dependency's
/// boxed future, the solve gets a current-thread runtime and a `LocalSet` on
/// a dedicated thread, and only its result crosses back.
async fn solve_on_dedicated_thread(
    docker: &Docker,
    tag: &str,
    frontend: bollard::grpc::build::ImageBuildFrontendOptions,
    load: bollard::grpc::build::ImageBuildLoadInput,
) -> Result<Result<(), GrpcError>> {
    use bollard::grpc::driver::Build;
    use bollard::grpc::driver::moby::Moby;

    let docker = docker.clone();
    let tag = tag.to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let outcome = (|| -> Result<Result<(), GrpcError>> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("failed to build a runtime for the buildkit solve")?;
            let local = tokio::task::LocalSet::new();
            Ok(local.block_on(&runtime, async move {
                Moby::new(&docker)
                    .docker_build(&tag, frontend, load, None, None)
                    .await
            }))
        })();
        let _ = tx.send(outcome);
    });
    rx.await.context("buildkit solve thread panicked")?
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

pub struct RunOpts<'a> {
    pub name: &'a str,
    pub network: &'a str,
    pub aliases: &'a [String],
    pub env: &'a [String],
    /// container-side ports to publish, each with an optional fixed
    /// host-side port — `None` publishes to a random ephemeral localhost
    /// port (the default), `Some(p)` binds exactly `127.0.0.1:p`.
    pub ports: &'a [(String, Option<u16>)],
    pub image: &'a str,
    /// Overrides the image's default `CMD` when non-empty — see `#Service`'s
    /// and `#BackingDependency`'s `command` doc comments in
    /// `schema/*.cue`. Empty leaves the image's own `CMD` untouched.
    pub command: &'a [String],
    /// stack/project id — mirrors Docker Compose's `com.docker.compose.project`
    /// label so Docker Desktop (and `docker ps`/`compose ls` tooling) groups
    /// every container in a run together, even though we never call `docker compose`.
    pub project: &'a str,
    pub service_name: &'a str,
    /// Pre-formatted `HostConfig.binds` entries — either a bind mount
    /// (`host/path:container/path[:ro]`) or a named volume
    /// (`volume-name:container/path[:ro]`). Docker itself disambiguates the
    /// two by whether the left side contains a `/`, so both forms share this
    /// one field.
    pub binds: &'a [String],
    /// `#RunOptions.restart` — see `docker::restart_policy_name`.
    pub restart_policy: &'a str,
    /// `#RunOptions.stop_signal` — `ContainerCreateBody.StopSignal`. `None`
    /// leaves the image's own `STOPSIGNAL` alone.
    pub stop_signal: Option<&'a str>,
    /// `#RunOptions.stop_grace_period` — `ContainerCreateBody.StopTimeout`,
    /// in seconds. Recorded on the container rather than passed to each
    /// `stop_container` call so that every path that ever stops this
    /// container honours it, including ones that no longer have a `Node` to
    /// read: an orphan whose node was deleted, a container that outlived the
    /// daemon, a `docker stop` typed by hand.
    pub stop_grace_period: u64,
    pub user: Option<&'a str>,
    pub working_dir: Option<&'a str>,
    /// User-declared labels — merged under fghj's own `com.docker.compose.*`
    /// labels below, which always win on key conflict.
    pub labels: &'a BTreeMap<String, String>,
    pub cap_add: &'a [String],
    pub cap_drop: &'a [String],
    pub privileged: bool,
    /// Pre-formatted `HostConfig.extra_hosts` entries (`"hostname:ip"`).
    pub extra_hosts: &'a [String],
    /// `HostConfig.dns` — resolver IPs to use instead of Docker's default,
    /// tried in order. Every node gets the run's sidecar first (authoritative
    /// for the `fghj.internal` zone and any active alias) and Docker's own
    /// embedded resolver (`127.0.0.11`) second as a fallback. Empty leaves
    /// Docker's default (embedded resolver only) untouched.
    pub dns: &'a [String],
    pub healthcheck: Option<&'a Healthcheck>,
    /// Pins the image's platform for `create_container`'s platform-aware
    /// image lookup (`os[/arch[/variant]]`, e.g. "linux/amd64"). fghj doesn't
    /// pull images itself today, so this only helps when the requested
    /// platform's image is already present locally.
    pub platform: Option<&'a str>,
}

pub async fn run_container(docker: &Docker, opts: &RunOpts<'_>) -> Result<()> {
    // User-declared labels first, so fghj's own bookkeeping labels below
    // always win on a key conflict — see `RunOpts.labels`'s doc comment.
    let mut labels: HashMap<String, String> = opts
        .labels
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    labels.insert(
        "com.docker.compose.project".to_string(),
        opts.project.to_string(),
    );
    labels.insert(
        "com.docker.compose.service".to_string(),
        opts.service_name.to_string(),
    );
    labels.insert("com.docker.compose.oneoff".to_string(), "False".to_string());

    let mut exposed_ports = Vec::new();
    let mut port_bindings = HashMap::new();
    for (port, host_port) in opts.ports {
        let container_port = port.split('/').next().unwrap_or(port);
        let key = format!("{container_port}/tcp");
        exposed_ports.push(key.clone());
        port_bindings.insert(
            key,
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".to_string()),
                host_port: host_port.map(|p| p.to_string()),
            }]),
        );
    }

    let mut endpoints_config = HashMap::new();
    endpoints_config.insert(
        opts.network.to_string(),
        EndpointSettings {
            aliases: Some(opts.aliases.to_vec()),
            ..Default::default()
        },
    );

    let body = ContainerCreateBody {
        image: Some(opts.image.to_string()),
        cmd: if opts.command.is_empty() {
            None
        } else {
            Some(opts.command.to_vec())
        },
        env: Some(opts.env.to_vec()),
        labels: Some(labels),
        exposed_ports: Some(exposed_ports),
        user: opts.user.map(|s| s.to_string()),
        working_dir: opts.working_dir.map(|s| s.to_string()),
        stop_signal: opts.stop_signal.map(|s| s.to_string()),
        stop_timeout: Some(opts.stop_grace_period as i64),
        healthcheck: opts.healthcheck.map(|hc| HealthConfig {
            test: Some(hc.test.clone()),
            interval: hc.interval.map(|s| (s * 1_000_000_000) as i64),
            timeout: hc.timeout.map(|s| (s * 1_000_000_000) as i64),
            start_period: hc.start_period.map(|s| (s * 1_000_000_000) as i64),
            retries: hc.retries.map(|r| r as i64),
            ..Default::default()
        }),
        host_config: Some(HostConfig {
            port_bindings: Some(port_bindings),
            binds: if opts.binds.is_empty() {
                None
            } else {
                Some(opts.binds.to_vec())
            },
            restart_policy: Some(RestartPolicy {
                name: Some(restart_policy_name(opts.restart_policy)),
                maximum_retry_count: None,
            }),
            cap_add: if opts.cap_add.is_empty() {
                None
            } else {
                Some(opts.cap_add.to_vec())
            },
            cap_drop: if opts.cap_drop.is_empty() {
                None
            } else {
                Some(opts.cap_drop.to_vec())
            },
            privileged: Some(opts.privileged),
            extra_hosts: if opts.extra_hosts.is_empty() {
                None
            } else {
                Some(opts.extra_hosts.to_vec())
            },
            dns: if opts.dns.is_empty() {
                None
            } else {
                Some(opts.dns.to_vec())
            },
            ..Default::default()
        }),
        networking_config: Some(NetworkingConfig {
            endpoints_config: Some(endpoints_config),
        }),
        ..Default::default()
    };

    let mut create_opts_builder = CreateContainerOptionsBuilder::default().name(opts.name);
    if let Some(platform) = opts.platform {
        create_opts_builder = create_opts_builder.platform(platform);
    }
    let create_opts = create_opts_builder.build();
    let result = async {
        docker.create_container(Some(create_opts), body).await?;
        docker.start_container(opts.name, None).await?;
        Ok::<(), bollard::errors::Error>(())
    }
    .await;

    if let Err(e) = result {
        // Docker can leave a container behind in `Created` state if start
        // fails post-create (e.g. network attach failure) — best-effort clean
        // it up so callers never leak a dangling container blocking retries.
        let _ = docker
            .remove_container(
                opts.name,
                Some(RemoveContainerOptionsBuilder::default().force(true).build()),
            )
            .await;
        return Err(e).with_context(|| format!("docker run {} failed", opts.name));
    }
    Ok(())
}

/// Stops a container without removing it — the counterpart to
/// `stop_and_remove` below, used by `RunRegistry::stop_container` so a
/// single node can be paused without losing the container (its logs, its
/// exact identity for a later plain restart) the way a full remove would.
pub async fn stop_container(docker: &Docker, name: &str) {
    let _ = docker.stop_container(name, None).await;
}

/// Stops a container and then removes it — two calls, deliberately, not one
/// `remove_container(force: true)`.
///
/// `force` is SIGKILL with no grace period at all, and this function is on
/// ordinary paths: stopping a whole run, restarting a single node,
/// reconciling a container whose config changed. A dev database with an
/// hour of seeded state in a `scope: stable` volume would be killed
/// mid-write by a routine action — the opposite of what a stable volume
/// promises. So stop first and let the container's own `StopSignal` /
/// `StopTimeout` (stamped on at create time by `run_container`) run their
/// course; Docker escalates to SIGKILL itself once the grace period expires,
/// so this cannot hang indefinitely.
///
/// The remove still passes `force`, for the case the stop could not finish:
/// a container that is somehow still running must not survive a teardown and
/// hold its name, network or volumes hostage from the next start.
pub async fn stop_and_remove(docker: &Docker, name: &str) {
    let _ = docker.stop_container(name, None).await;
    let _ = docker
        .remove_container(
            name,
            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
        )
        .await;
}

pub struct ContainerStatus {
    pub status: String,
    pub published_port: Option<u16>,
    /// The process's exit code, once Docker has one to report — `None`
    /// while the container is still running (and for a container that never
    /// ran). The reading a terminating node (`Node.kind == "task"`, see
    /// `runs::health::wait_for_exit`) is actually waiting on: for a
    /// long-running service `status` alone answers every question, but for a
    /// container whose entire purpose is to finish, "exited" is only half
    /// the answer.
    pub exit_code: Option<i64>,
}

/// Inspects a container, returning its status and the host port bound to
/// `container_port` (e.g. "8080" or "8080/tcp"), if published. Returns
/// `Ok(None)` if the container doesn't exist, rather than erroring — this is
/// used as the "does it exist" check everywhere.
pub async fn inspect_status(
    docker: &Docker,
    name: &str,
    container_port: &str,
) -> Result<Option<ContainerStatus>> {
    let inspected = match docker
        .inspect_container(
            name,
            Some(InspectContainerOptionsBuilder::default().build()),
        )
        .await
    {
        Ok(entry) => entry,
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => return Ok(None),
        Err(e) => return Err(e).context("docker inspect_container failed"),
    };

    let (status, exit_code) = match &inspected.state {
        Some(state) => (
            state
                .status
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            state.exit_code,
        ),
        None => ("unknown".to_string(), None),
    };

    let port_key = if container_port.contains('/') {
        container_port.to_string()
    } else {
        format!("{container_port}/tcp")
    };
    let published_port = inspected
        .network_settings
        .and_then(|n| n.ports)
        .and_then(|p| p.get(&port_key).cloned().flatten())
        .and_then(|bindings| bindings.into_iter().next())
        .and_then(|b| b.host_port)
        .and_then(|p| p.parse::<u16>().ok());

    Ok(Some(ContainerStatus {
        status,
        published_port,
        exit_code,
    }))
}

/// A container's own IP address on one specific docker network — nothing
/// today exposes this; `RunOpts.aliases`/Docker's embedded per-network DNS
/// covers every existing need to *reach* a container by name, but the
/// sidecar-proxy `extra_hosts` sentinel (see `runs/`'s
/// `rewrite_extra_hosts_sentinel`) needs the sidecar's raw IP to hand to
/// *other* containers via `HostConfig.extra_hosts`, which takes literal IPs,
/// not names. Returns `Ok(None)` if the container doesn't exist or isn't
/// attached to `network` (same not-found-is-fine convention as
/// `inspect_status`).
pub async fn inspect_network_ip(
    docker: &Docker,
    name: &str,
    network: &str,
) -> Result<Option<String>> {
    let inspected = match docker
        .inspect_container(
            name,
            Some(InspectContainerOptionsBuilder::default().build()),
        )
        .await
    {
        Ok(entry) => entry,
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => return Ok(None),
        Err(e) => return Err(e).context("docker inspect_container failed"),
    };

    Ok(inspected
        .network_settings
        .and_then(|n| n.networks)
        .and_then(|mut networks| networks.remove(network))
        .and_then(|endpoint| endpoint.ip_address)
        .filter(|ip| !ip.is_empty()))
}

/// Inspects a container's declared `HEALTHCHECK` status ("starting",
/// "healthy", "unhealthy"), if it has one. Returns `Ok(None)` both when the
/// container doesn't exist (404, same convention as `inspect_status`) and
/// when it exists but declares no healthcheck at all — callers that need to
/// tell those two cases apart should call `inspect_status` too.
pub async fn inspect_health(docker: &Docker, name: &str) -> Result<Option<String>> {
    let inspected = match docker
        .inspect_container(
            name,
            Some(InspectContainerOptionsBuilder::default().build()),
        )
        .await
    {
        Ok(entry) => entry,
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => return Ok(None),
        Err(e) => return Err(e).context("docker inspect_container failed"),
    };

    Ok(inspected
        .state
        .and_then(|s| s.health)
        .and_then(|h| h.status)
        .map(|s| s.to_string()))
}

/// One-shot: fetches the last `tail` lines and returns them as a string.
pub async fn logs_tail(docker: &Docker, name: &str, tail: usize) -> Result<String> {
    let options = LogsOptionsBuilder::default()
        .stdout(true)
        .stderr(true)
        .tail(&tail.to_string())
        .build();
    let mut stream = docker.logs(name, Some(options));
    let mut combined = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => combined.push_str(&chunk.to_string()),
            Err(e) => {
                combined.push_str(&format!("[fghj: log read error: {e}]\n"));
                break;
            }
        }
    }
    Ok(combined)
}

/// Continuous: follows the container's log output, never ending until the
/// container stops or the caller drops the stream. Used by the SSE endpoint.
///
/// Bollard's `Docker::logs` clones what it needs out of `&self` before
/// returning (see its `process_request`), so the returned stream owns
/// everything it needs and isn't tied to `docker`'s or `name`'s lifetime —
/// safe to return from a handler after `docker`/`name` go out of scope.
pub fn logs_follow(
    docker: &Docker,
    name: &str,
    timestamps: bool,
) -> impl Stream<Item = Result<bollard::container::LogOutput, bollard::errors::Error>> + use<> {
    let options = LogsOptionsBuilder::default()
        .follow(true)
        .stdout(true)
        .stderr(true)
        .tail("0")
        .timestamps(timestamps)
        .build();
    docker.logs(name, Some(options))
}

/// A live `docker exec` session — `output` and `input` are independent
/// halves of one duplex connection (Docker upgrades the HTTP connection to a
/// raw byte stream once attached), so both directions can be driven
/// concurrently by a `tokio::select!` loop. See `bollard::exec::start_exec`'s
/// `StartExecResults::Attached` variant, which this wraps.
pub struct ExecSession {
    pub id: String,
    pub output: Pin<
        Box<
            dyn Stream<Item = Result<bollard::container::LogOutput, bollard::errors::Error>> + Send,
        >,
    >,
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
}

/// Starts a command inside an already-running container and attaches to it,
/// full duplex. `tty` merges stdout/stderr into one undifferentiated stream
/// (real terminal semantics) — same simplification `fghj exec` relies on to
/// avoid stream-tagging both with and without a TTY.
pub async fn exec_start(
    docker: &Docker,
    container_name: &str,
    cmd: &[String],
    user: Option<&str>,
    working_dir: Option<&str>,
    tty: bool,
) -> Result<ExecSession> {
    let create_opts = CreateExecOptions {
        attach_stdin: Some(true),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        tty: Some(tty),
        cmd: Some(cmd.to_vec()),
        user: user.map(|s| s.to_string()),
        working_dir: working_dir.map(|s| s.to_string()),
        ..Default::default()
    };
    let created = docker
        .create_exec(container_name, create_opts)
        .await
        .context("docker create_exec failed")?;

    let started = docker
        .start_exec(
            &created.id,
            Some(StartExecOptions {
                detach: false,
                tty,
                ..Default::default()
            }),
        )
        .await
        .context("docker start_exec failed")?;

    match started {
        StartExecResults::Attached { output, input } => Ok(ExecSession {
            id: created.id,
            output,
            input,
        }),
        // We always pass `detach: false` above, so Docker never takes the
        // detached path — `start_exec` only returns `Detached` when the
        // caller asks for it.
        StartExecResults::Detached => bail!("docker start_exec unexpectedly detached"),
    }
}

/// Resizes an exec session's pseudo-TTY — a no-op error from Docker's side
/// if the session wasn't started with `tty: true`, so callers only need to
/// call this when they know they allocated one.
pub async fn exec_resize(docker: &Docker, exec_id: &str, cols: u16, rows: u16) -> Result<()> {
    docker
        .resize_exec(
            exec_id,
            ResizeExecOptions {
                width: cols,
                height: rows,
            },
        )
        .await
        .context("docker resize_exec failed")
}

/// The exec's exit code, once its process has finished. `-1` if Docker
/// doesn't report one (shouldn't happen for a completed exec, but this is
/// simpler than propagating a second failure mode this far into a stream's
/// end for something purely informational).
pub async fn exec_exit_code(docker: &Docker, exec_id: &str) -> Result<i64> {
    let inspected = docker
        .inspect_exec(exec_id)
        .await
        .context("docker inspect_exec failed")?;
    Ok(inspected.exit_code.unwrap_or(-1))
}

#[cfg(test)]
mod tests {

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
    use super::*;
    use std::process::Command;
    use tokio::io::AsyncWriteExt;

    /// A throwaway `busybox` container, torn down on drop — exists purely so
    /// exec tests below have a real running container to attach to, without
    /// pulling in the resolver/`RunOpts` machinery `run_container` needs.
    struct TestContainer {
        name: String,
    }

    impl TestContainer {
        fn start() -> Self {
            let name = format!(
                "fghj-exec-test-{}",
                std::process::id().wrapping_add(rand_suffix())
            );
            let status = Command::new("docker")
                .args([
                    "run", "-d", "--rm", "--name", &name, "busybox", "sleep", "60",
                ])
                .status()
                .expect("failed to run `docker run` for exec test fixture");
            assert!(status.success(), "docker run failed for exec test fixture");
            Self { name }
        }
    }

    impl Drop for TestContainer {
        fn drop(&mut self) {
            let _ = Command::new("docker")
                .args(["rm", "-f", &self.name])
                .status();
        }
    }

    /// Cheap, dependency-free uniqueness for the container name — tests in
    /// this module never run concurrently with each other in practice, but a
    /// stale container from a previous crashed run shouldn't collide either.
    pub(super) fn rand_suffix() -> u32 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn exec_start_streams_output_and_reports_exit_code() {
        let docker = crate::daemon::connect_docker().expect("docker client");
        let container = TestContainer::start();

        let mut session = exec_start(
            &docker,
            &container.name,
            &["echo".to_string(), "hello from exec".to_string()],
            None,
            None,
            false,
        )
        .await
        .expect("exec_start failed");

        let mut collected = Vec::new();
        while let Some(item) = session.output.next().await {
            collected.extend_from_slice(&item.expect("exec output stream error").into_bytes());
        }
        let text = String::from_utf8_lossy(&collected);
        assert!(
            text.contains("hello from exec"),
            "unexpected exec output: {text}"
        );

        let code = exec_exit_code(&docker, &session.id)
            .await
            .expect("exec_exit_code failed");
        assert_eq!(code, 0);
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn exec_start_reports_nonzero_exit_code_and_accepts_stdin() {
        let docker = crate::daemon::connect_docker().expect("docker client");
        let container = TestContainer::start();

        // `cat` echoes stdin back to stdout, then exits 0 once stdin closes —
        // exercises the duplex `input` half, not just `output`.
        let mut session = exec_start(
            &docker,
            &container.name,
            &["cat".to_string()],
            None,
            None,
            false,
        )
        .await
        .expect("exec_start failed");

        session
            .input
            .write_all(b"round trip\n")
            .await
            .expect("failed to write exec stdin");
        session
            .input
            .shutdown()
            .await
            .expect("failed to close exec stdin");

        let mut collected = Vec::new();
        while let Some(item) = session.output.next().await {
            collected.extend_from_slice(&item.expect("exec output stream error").into_bytes());
        }
        assert_eq!(String::from_utf8_lossy(&collected), "round trip\n");
        assert_eq!(
            exec_exit_code(&docker, &session.id).await.unwrap(),
            0,
            "cat should exit 0 once stdin closes"
        );

        // A second exec against the same still-running container that exits
        // nonzero — confirms exit codes aren't always trivially 0/-1.
        let mut failing = exec_start(
            &docker,
            &container.name,
            &["sh".to_string(), "-c".to_string(), "exit 7".to_string()],
            None,
            None,
            false,
        )
        .await
        .expect("exec_start failed");
        while (failing.output.next().await).is_some() {}
        assert_eq!(exec_exit_code(&docker, &failing.id).await.unwrap(), 7);
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
            let tag = format!("fghj-build-test/{label}:{}", super::tests::rand_suffix());
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
                tag: &fixture.tag,
                platform: None,
                args: &args,
                target: Some("wanted"),
                secrets: &[],
                ssh_auth_sock: None,
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
                tag: &fixture.tag,
                platform: None,
                args: &BTreeMap::new(),
                target: None,
                secrets: &[(String::from("token"), secret_path)],
                ssh_auth_sock: None,
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
                tag: &fixture.tag,
                platform: None,
                args: &BTreeMap::new(),
                target: None,
                secrets: &[(String::from("token"), secret_path)],
                ssh_auth_sock: None,
            },
        )
        .await
        .expect_err("a RUN that exits 7 must fail the build");
        assert!(
            format!("{err:#}").contains("buildkit build"),
            "error should name the build it came from, got: {err:#}"
        );
    }
}
