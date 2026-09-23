//! Process bootstrap: connecting to Docker and bringing the whole control
//! API — listeners, CA, proxy, DNS, reconcilers — up and down.

use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::daemon::api::build_router;
use crate::daemon::control::DaemonControl;
use crate::daemon::reconcile::{spawn_reconciler, spawn_sync_reconciler};
use crate::daemon::registry::WorkspaceRegistry;
use crate::daemon::{ca_dir, socket_path};
use crate::{ca, daemon_log, dns, persistence};

/// Connects to the Docker Engine API, preferring the plain `DOCKER_HOST`/
/// default-socket convention bollard understands natively, but falling back
/// to whatever socket the `docker` CLI's *active context* actually points
/// at. Docker Desktop, OrbStack, colima, etc. all route the `docker` command
/// through a context rather than the classic `/var/run/docker.sock` — a
/// concept bollard has no notion of — so without this fallback `fghjd` would
/// fail to connect on exactly the setups where `docker <cmd>` works fine.
pub fn connect_docker() -> Result<bollard::Docker> {
    match bollard::Docker::connect_with_local_defaults() {
        Ok(docker) => Ok(docker),
        Err(default_err) => {
            let context_host = Command::new("docker")
                .args([
                    "context",
                    "inspect",
                    "--format",
                    "{{.Endpoints.docker.Host}}",
                ])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|h| !h.is_empty());
            match context_host {
                Some(host) => {
                    bollard::Docker::connect_with_socket(&host, 120, bollard::API_DEFAULT_VERSION)
                        .with_context(|| {
                            format!("failed to connect to the docker context's socket ({host})")
                        })
                }
                None => Err(default_err).context("failed to construct a Docker client"),
            }
        }
    }
}

/// Connects to the Docker Engine API over its local socket, stands up DNS
/// (Subsystem B) and the TLS reverse proxy (Subsystem C) in front of the
/// control API, and serves the control API/UI forever. Fails fast if Docker
/// isn't reachable or any of the fixed/privileged ports (80, 443) or the
/// system trust store can't be bound/installed, rather than letting that
/// surface confusingly on the first request.
///
/// `fghjd` itself never exits on its own after this point except via a
/// terminating signal (SIGTERM/SIGINT) — the intent is that it's started at
/// boot and supervised (launchd/systemd), restarting on crash, for the life
/// of the machine. `fghj daemon stop`/`start` (see `DaemonControl`) toggle
/// whether it's actively occupying ports/DNS/`/etc/hosts` without touching
/// this process's lifecycle at all; a real signal is reserved for an actual
/// shutdown (service uninstall/restart, system shutdown), at which point we
/// still deactivate first so we don't leave stale ports/hosts entries behind
/// for whatever comes next.
pub async fn run_control_api() -> Result<()> {
    let docker = connect_docker()?;
    docker
        .ping()
        .await
        .context("failed to reach the Docker daemon over its socket — is Docker running?")?;
    let docker = Arc::new(docker);

    // The control API's OS-assigned TCP port is what the HTTPS proxy relays
    // `https://fghj.internal` to internally (see `proxy::serve_https`'s
    // `control_port` param) — never dialed directly by anything else, so it
    // doesn't need to be fixed or discoverable. Bound before anything else so
    // there's something for the proxy to relay to even if activation below
    // fails partway.
    let control_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("failed to bind control API")?;
    let control_port = control_listener
        .local_addr()
        .context("control API socket has no local address")?
        .port();

    // The `fghj` CLI talks to the same control API over a Unix socket
    // instead — dockerd-style: no port to pick or discover, and access is a
    // filesystem permission rather than "anything that can reach
    // 127.0.0.1". `fghjd` runs as root while `fghj` runs as the invoking
    // user, so the socket needs opening up beyond its default root-only
    // permissions for the CLI to reach it at all.
    let socket_path = socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let cli_listener = tokio::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind control socket {}", socket_path.display()))?;
    std::fs::set_permissions(
        &socket_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o666),
    )
    .with_context(|| format!("failed to set permissions on {}", socket_path.display()))?;

    let cert_path = ca::ca_cert_path(&ca_dir());
    let ca = {
        let dir = ca_dir();
        tokio::task::spawn_blocking(move || ca::ensure_ca(&dir))
            .await
            .context("CA setup task panicked")??
    };
    tokio::task::spawn_blocking(move || ca::install_macos_trust(&cert_path))
        .await
        .context("CA trust install task panicked")??;
    // `cert.pem`/`bundle.pem` under the same dir: the whole mechanism by
    // which a container trusts fghj's zone, via a plain `volumes:` mount in
    // its own `.fghj.yaml` — see `ca::refresh_trust_files`. The CA itself
    // never rotates at runtime, so this only needs to run once here, not on
    // a periodic reconciler.
    ca::refresh_trust_files(&ca_dir(), &ca).context("failed to refresh CA trust files")?;

    // Best-effort and non-blocking: not every workspace ends up starting a
    // run before `fghjd` itself might need to restart, so a slow/failed
    // build here (first build compiles the whole crate, and needs crates.io
    // reachable) shouldn't hold up `fghjd` starting or fail it outright — a
    // run that actually needs its sidecar will surface a real error from
    // `RunRegistry::ensure_sidecar`'s own call to this same function.
    {
        let docker = docker.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::sidecar_image::ensure_built(&docker).await {
                daemon_log::warn(format!(
                    "fghjd: failed to pre-build the sidecar proxy image: {e:#}"
                ));
            }
        });
    }

    let provider = Arc::new(rustls::crypto::ring::default_provider());

    // Loaded before the cert resolver: the cert resolver needs it too, to
    // gate certificate issuance for a reserved-TLD `#AdditionalHost` alias on
    // "is some running container actually claiming this name as a route"
    // (see `ca::DynamicCertResolver::resolve_for`), not just "is this in our
    // own zone".
    let registry = Arc::new(WorkspaceRegistry::load(docker).await);

    let cert_resolver = Arc::new(ca::DynamicCertResolver::new(
        ca,
        provider.clone(),
        registry.clone(),
    ));

    let daemon_state_path = persistence::default_state_path();
    let idle_requested = persistence::load_daemon_state(&daemon_state_path).idle_requested;
    let daemon = Arc::new(DaemonControl {
        registry: registry.clone(),
        cert_resolver,
        provider,
        control_port,
        active: Mutex::new(None),
        last_reconcile_ms: Mutex::new(None),
        idle_requested: Mutex::new(idle_requested),
        daemon_state_path,
    });
    spawn_reconciler(Arc::clone(&daemon));
    spawn_sync_reconciler(Arc::clone(&daemon));

    // `fghjd` starts active by default: it's meant to occupy 80/443 and
    // *.fghj.internal DNS from the moment the system boots. The one
    // exception is `daemon.is_idle_requested()` — if the operator's last
    // explicit `fghj daemon` call was `stop`, a crash or reboot in between
    // must not silently override that by reactivating anyway; staying idle
    // here is what makes `fghj daemon stop` a durable instruction rather
    // than a one-shot action that a flaky Docker daemon or a reboot can undo
    // behind the operator's back. A bind failure during activation (e.g.
    // "something else is already listening on 80/443") still fails startup
    // fast, before the control API ever serves a request.
    if daemon.is_idle_requested() {
        daemon_log::info(
            "fghjd: starting idle — last `fghj daemon` action was `stop`; run `fghj daemon start` to reconcile"
                .to_string(),
        );
    } else {
        daemon.activate().await?;
    }

    let app = build_router(registry, Arc::clone(&daemon));
    daemon_log::info(format!(
        "fghjd: control API listening on {} (CLI) and reachable via https://{}",
        socket_path.display(),
        dns::ZONE
    ));

    // Same router, two listeners: the Unix socket is the CLI's channel, the
    // TCP one is only ever dialed internally by the HTTPS proxy's apex-name
    // relay (see above) — `Router` is cheap to clone (an `Arc` internally).
    let cli_app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(cli_listener, cli_app).await {
            daemon_log::warn(format!("fghjd: control socket server error: {e}"));
        }
    });

    let shutdown_signal = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("failed to install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
    };

    tokio::select! {
        result = axum::serve(control_listener, app) => {
            result.context("control API server error")?;
        }
        _ = shutdown_signal => {
            daemon_log::info(
                "fghjd: received shutdown signal, releasing ports and cleaning up...".to_string(),
            );
            daemon.deactivate();
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}
